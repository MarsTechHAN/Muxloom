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
    path::{Path, PathBuf},
    sync::{Mutex, MutexGuard},
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};

use crate::{
    config::{Config, McpConfig, State, default_state_path},
    daemon_protocol::{DaemonSession, Trigger, TriggerAction},
    model::{
        AgentKind, FilePreview, FilePreviewKind, HistorySearchHit, LaunchRequest, Powers, Reach,
        Target,
    },
    relay::now_ms,
    runtime::{Runtime, ScreenRead},
    ssh_config::{self, MANAGED_INCLUDE, ManagedHosts},
    talk::{
        MAX_TEXT, TalkAddress, TalkAuthor, TalkDeliver, TalkDraft, TalkFilter, TalkKind,
        TalkMessage, TalkPage, TalkScope, TalkSelector, TalkState, TalkVector, TalkVoice,
        decode_cursor, encode_cursor, hostname, paste_bytes,
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
///
/// `call` borrows immutably: a serve loop may answer several requests at once
/// on separate threads (a long `wait_for` must not hold up a `read_screen`),
/// so one surface must be callable concurrently. Implementations that keep
/// mutable state hold it behind their own locks.
pub trait ControlSurface: Send + Sync {
    fn tools(&self) -> Vec<ToolSpec>;
    fn call(&self, name: &str, arguments: &Value) -> Result<String>;

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
/// How long a wait runs when the caller does not say, and the most a single
/// call may block. A wait-class call is one long-lived JSON-RPC request, and
/// an MCP client (opencode's, and the SDK default) drops any request it has
/// not heard back from in ~60 seconds, answering `-32001: Request timed out`.
/// A call that returns at 45 seconds saying "not yet, call again" is always
/// answerable; one the client gives up on is not. Both wait tools already tell
/// the caller to re-poll, so the cap only changes how the wait is sliced.
const WAIT_DEFAULT_TIMEOUT_SECONDS: u64 = 45;
const WAIT_MAX_TIMEOUT_SECONDS: u64 = 45;
/// Consecutive failed looks a wait rides out before reporting them. A daemon
/// handing over to a new generation is unreachable for a moment, and a wait
/// that outlives conversations must outlive that too.
const WAIT_ERROR_TOLERANCE: usize = 3;
/// The longest a `talk_read` may sit in a single call, and how often it looks
/// while it waits. Shares `wait_for`'s ceiling because it is the same kind of
/// long-lived request: a `talk_read` that blocks past the MCP client's
/// ~60-second request timeout is dropped with `-32001` before it can answer
/// "nothing yet, here is your cursor". Each call therefore returns within the
/// 45-second cap and the caller re-polls — the tool text and the skill both
/// say to call again while an answer is outstanding — rather than hold one
/// call open until the transport gives up on it.
const TALK_MAX_WAIT_SECONDS: u64 = WAIT_MAX_TIMEOUT_SECONDS;
const TALK_POLL: Duration = Duration::from_secs(2);
/// How much of the board one read may hand back at once.
///
/// `limit` bounds the page in messages, and a message has no length of its
/// own: fifty of them are a handful of lines on a quiet board and sixty
/// thousand characters on one where agents have been handing work to each
/// other. Past what the MCP client will accept the whole reply is refused, so
/// a caller who asked for the default page learns nothing at all — the worst
/// answer available, and the one a busy board gives most reliably. A page
/// that leaves its oldest few behind and says so is worth more than one that
/// does not arrive.
///
/// Counted in characters, not bytes. A board these agents actually use is
/// written in whatever language the people around it speak, and a byte ceiling
/// is three times tighter on a board written in Chinese than on the same board
/// written in English - so the cut fell on the boards that most needed not to
/// be cut. Thirty thousand characters is past any page a board worth reading
/// produces, which is the point: this is the ceiling that stops a reply being
/// refused outright, not a page size.
const TALK_MAX_RESPONSE_CHARS: usize = 30_000;
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
    "send_channel_message",
    "set_head_name",
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
             list_machines shows the others as `remote`: while the controller is attached the \
             whole fleet is yours to work in — name one with the `machine` argument and the \
             controller runs the call over there. Looking and saying go straight through; \
             starting a session, typing into one, ending one, or running a shell command is put \
             to the person first and runs once they say so. Only wait_for stays here, watching \
             this machine"
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
            "- A remote machine is only ever reached through the controller. If it is not \
             attached you are told so immediately — that is the whole answer, not a reason to \
             retry. A change over there waits on the person: you get back an approval id rather \
             than a result, and the call runs when they answer, so make it again after they do. \
             When there is already an agent living over there, saying what you want with \
             message_agent is quicker than driving its machine from here.\n"
        }
    };
    // Only a session has a head name of its own: the daemon flavor runs inside one,
    // while the controller is the user's fleet-wide view and has no row to name.
    let headname = match flavor {
        Flavor::Daemon => {
            "- Keep your head name honest: set_head_name {{ name: \"...\" }} updates the name on \
             your row in the dashboard, so a person watching sees your current task at a glance. \
             Update it when the shape of your work changes, not on every step; keep it a whole- \
             task phrase under 60 characters.\n"
        }
        Flavor::Controller => "",
    };
    let channel_limit = crate::channel::READABLE_LIMIT;
    let mut text = format!(
        "muxloom manages long-lived terminal sessions — Codex, Claude Code, pi, OpenCode, and \
         plain shells — \
         {reach}. Sessions outlive this conversation and the muxloom dashboard, and a human may \
         be watching any of them right now.\n\n\
         Work through the sessions rather than around them:\n\
          - To get work done on a machine, talk to the agent session that lives there: \
          message_agent to say something to another agent, send_input for raw keystrokes and for \
          plain shells, then wait_for or read_screen to see what came of it. Treat a session as a \
          colleague you are messaging, not as a subprocess you drive.\n\
          - Subagents are sessions too. To fan work out, launch one with launch_session so it \
          shows up in the dashboard and on the task board where you and the person watching can \
          both follow it. Do not reach for your harness's built-in subagent or task tool: those \
          are invisible, untracked, and die with your process.\n\
          - Being handed more than one task is the signal to fan out. Split the list before you \
          start, give each session a brief specific enough to need no follow-up question, and \
          keep the coordinating and the aggregating for yourself. Work them one after another \
          only when they genuinely depend on each other.\n\
          - A message lands in the session's prompt box and is read when its turn ends. For pi and \
         OpenCode the box is read off their screen like the others: if it already holds an \
         unsent sentence your message is held until it clears, and if a model or effort picker is \
         up it waits rather than answering the dialog. When unsure whether a message went in, \
         read_screen or check list_sessions' working/needs_attention before sending another.\n\
         - run_shell is a last resort. Reach for it only for a short, non-interactive, ideally \
         read-only query that no other tool covers. Never start long-running or interactive work \
         with it — that is what launch_session is for — and never use it to do something a \
         session you could talk to would do better.\n\
          - Prefer the narrow tools (list_sessions, read_screen, list_files, preview_file, \
          search_history) over shell equivalents: they are bounded, paged, and safe to repeat.\n\
          - read_screen returns the read result by default (chrome stripped, content in \
          reading order); pass raw: true for the raw vt100 grid.\n\n\
          Work with the others out in the open:\n\
          {headname}\
          - The talk board is the fleet's shared memory, not a chat. talk_read it once when you \
         pick up a piece of work, and search it when something surprises you: what is on it is \
         what other agents worked out and wrote down, on every machine, long after their \
         sessions ended. talk_post back to it only what will still be worth knowing when this \
         conversation is over — a decision and why, a gotcha and what it cost, a cause that took \
         real work to find. Status, progress, questions, answers, and anything true only for the \
         next hour do not go on it: your head name says what you are doing, message_agent \
         reaches the one agent who needs an answer, and send_channel_message reaches the person. \
         Post to the narrowest scope the knowledge is actually true in.\n\
         - message_agent is how you ask one agent for something: it lands in that session's \
         prompt in an envelope that names you, and it is read when the turn it is in ends. Its \
         answer comes back as a direct message — wait for it with talk_read {{ scope: \
         \"direct\", wait_seconds: {TALK_MAX_WAIT_SECONDS} }}, and call that again each time it \
         returns nothing rather than asking twice. Minutes is normal. When you are the one asked, \
         answer even if the answer is no: the agent waiting on you cannot act on silence.\n\
         - Nobody here is in charge of anyone else, and nothing you send has to be obeyed. Ask, \
         say why, and leave the other agent to judge it against what it is already doing.\n\
         - send_channel_message reaches the human themselves, on their phone, through the chat \
         app they bound to muxloom. It is for the end of something they are waiting on and for a \
         decision only they can make — one summary they can act on, never a progress log. Keep \
         it short: the text is capped at {channel_limit} characters and a longer one is refused \
         rather than trimmed, because a trim would silently take whatever you put last. Send the \
         conclusion, the numbers that matter, and the ask; say where the rest already is instead \
         of repeating it. Their answer comes back to you as a direct message. This and the talk \
         board are independent surfaces: a channel message posts nothing to the board, and a \
         talk_post tells the person on their phone nothing. Reply to a person on the surface \
         they wrote to — over the channel, not the board.\n\
         - When the person writes to you, answer before you start the work. They are on a phone \
         and cannot see your screen, so what you understood and roughly how long it will take is \
         worth a line straight away; the result follows when it is done. Silence for twenty \
         minutes reads exactly like an agent that never heard them.\n\n\
         Boundaries that are not negotiable:\n\
         - Machines the user has not enabled are unreachable. Naming one is an error, not a \
         workaround to route around.\n\
         - send_input is the only supported way to type. Never open a terminal stream just to \
         write bytes — it resizes the session under whoever is attached.\n\
         - Sessions are persistent state. A session you launched is yours to archive or delete \
         when it is done; a session you did not launch is someone else's — do not archive, \
         delete, or reconfigure it unless you were asked to.\n\
         - delete_session destroys a session's history irreversibly. Ask the human first; \
         archive_session keeps the history. Either one takes the whole fleet under that session \
         with it, so closing a master closes every subagent it started.\n\
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
                    "machine": { "type": "string", "description": "A machine id from list_machines: this machine's own name, or an SSH alias." },
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
        description: "List managed agent sessions with fresh status: kind (codex/claude/pi/\
                      opencode/terminal), working directory, whether the agent is working, \
                      whether it waits for input (needs_attention plus the matched reason), and \
                      a recap line. A terminal works while its shell is running something and \
                      waits when that program asks on its last row; it has no recap - \
                      read_screen is how a shell is read. Archived sessions are included only \
                      with include_archived."
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
            "Read a session's terminal. Returns the screen's read result by default: \
             the text a person would read off the screen — borders and the bottom \
             status/footer bar stripped, internal whitespace collapsed, content in \
             reading order, a line that wrapped joined back together. The visible \
             screen plus scrollback, up to `lines` rows (default {DEFAULT_SCREEN_LINES}) \
             ending `offset_from_bottom` rows above the last row anything was drawn on. \
             The trailer says how many rows the page holds, whether there is older \
             history above it, and the `offset_from_bottom` that reaches the page before \
             it — pass that back to page older output. A full-screen program (OpenCode) \
             is named as one: the terminal keeps nothing it drew before, so its earlier \
             turns are read with read_conversation. Pass `raw: true` for the raw vt100 \
             grid (ANSI stripped, column positions intact) when you need the exact \
             layout."
        ),
        input_schema: schema_across(
            flavor,
            json!({
                "session_id": { "type": "string", "description": "Session id from list_sessions." },
                "lines": { "type": "integer", "description": "Rows to return." },
                "offset_from_bottom": { "type": "integer", "description": "Rows above the last drawn row to end at; the previous page's trailer names the one that reaches the page before it." },
                "raw": { "type": "boolean", "description": "Return the raw vt100 grid instead of the read result." },
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
        input_schema: schema_across(
            flavor,
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
                      keystrokes rather than a message. The target must be a codex, claude, \
                      session; a terminal has nobody in it to read. Replies come back the same \
                      way, as direct messages: read yours with talk_read { scope: \"direct\" }, \
                      and answer with message_agent again. Never answer one of these with \
                      talk_post — that puts a private exchange on a board every agent on every \
                      machine has to read past. \
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
             send_input; to be told about something you will not be here for, use trigger.{}",
            match flavor {
                Flavor::Controller => "",
                // The one watcher that cannot travel: it would sit here
                // waiting on a session that is somewhere else.
                Flavor::Daemon =>
                    " It watches this machine only. For a session on another one, look with \
                     read_screen { machine } or list_sessions { machine } instead.",
            }
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
        input_schema: schema_across(
            flavor,
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
            "Read the talk board: the fleet's shared memory, written by every muxloom agent and \
             every person at a dashboard, on every machine, and kept long after the sessions \
             that wrote it are gone. Read it when you pick up a piece of work, to find out what \
             is already known about it — a decision somebody made, a gotcha somebody paid for, \
             where a surprising thing lives. It is knowledge rather than news, so it is worth \
             one read at the start and a search when you hit something odd; it is not worth \
             polling, and what the others are doing right now is in list_sessions, not here.\n\n\
             By default it shows what is in front of you — this machine's board, this \
             directory's board, what was written for everyone, and direct messages addressed to \
             you. `query` searches the text and is usually the better way in than reading the \
             lot; `include_machines` and `include_paths` widen the search to named machines and \
             directories, or to \"all\" to look everywhere. scope \"task\" narrows it to you, \
             whoever started you, and the subagents any of you started.\n\n\
             scope \"direct\" is the exception to all of that, and the one thing here worth \
             waiting on. It is not one of the four axes and not the board: it is the mailbox of \
             what agents said straight to each other, filed beside the board rather than on it \
             and kept hours rather than weeks. It is read through this tool because that is \
             where a reply to your message_agent arrives. `since_cursor` takes the `cursor` from \
             a previous read and returns only what has happened since, so polling never repeats \
             itself; `wait_seconds` (up to {TALK_MAX_WAIT_SECONDS}) holds the call open until \
             something new is said, which is how you wait to be answered. A wait that ends with \
             nothing is not an answer of no: it comes back with `waiting_on`, listing which of \
             your own messages are still unanswered and what the sessions holding them are \
             doing, and calling it again is usually right.\n\n\
             A reply too large to hand back is cut, and says so in `truncated` and `note`. \
             Following a cursor keeps the oldest of what is new and holds the cursor back to \
             match, so read again with the cursor and the rest follows in order; a read without \
             one keeps the newest, and `before` pages back from there."
        ),
        input_schema: schema(
            multi,
            json!({
                "scope": { "type": "string", "enum": ["path", "machine", "task", "global", "direct"], "description": "Only one kind of board. Default: all of them." },
                "since_cursor": { "type": "string", "description": "Cursor from an earlier read: return only what has been said since." },
                "wait_seconds": { "type": "integer", "description": "Wait this long for something new before answering. Default 0." },
                "limit": { "type": "integer", "description": "Most messages to return. Default 50. A page too large to hand back is cut whatever this says." },
                "before": { "type": "integer", "description": "Epoch ms: read backwards from here, for paging into the past." },
                "kinds": { "type": "array", "items": { "type": "string", "enum": ["note", "message", "direct"] }, "description": "\"note\" is what agents write down, \"message\" the older kind and what a person types at the dashboard, \"direct\" what agents said straight to each other." },
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
        description: "Write something down on the talk board: the fleet's shared memory, kept for \
                      weeks, replicated to every machine, and read by agents who will never meet \
                      you. It is a memory, not a transport. Post only what will still be worth \
                      knowing after the conversation that learned it has ended — a decision and \
                      why it went that way, a gotcha and what it costs, where something \
                      surprising lives, a cause you spent an hour finding, an approach that was \
                      tried and does not work. Write it for someone who is not here yet and has \
                      no idea what you were doing: one self-contained paragraph, no pronouns \
                      pointing at things only you can see. If a thing you are about to post \
                      would be stale in an hour, it does not belong here.\n\n\
                      These do not go on the board, ever: anything meant for one agent, or \
                      anything you want an answer to — that is message_agent, and it actually \
                      reaches them; status, progress, and what you are working on right now — \
                      that is your head name, set_head_name; a question, an answer, or a \
                      back-and-forth of any kind — message_agent again, because a board carrying \
                      two agents' conversation is a board everyone else stops reading; anything \
                      the person should see — send_channel_message, on their phone; and anything \
                      already written down in the code, the diff, the commit message, or the CI \
                      run, which needs at most a note saying where it is. Posting interrupts \
                      nobody and reaches nobody in particular. If what you have is news rather \
                      than knowledge, that is the sign not to post it.\n\n\
                      What the board takes is a note, and these are refused rather than advised \
                      against, so it stays a memory whoever is writing to it. A post that opens \
                      by naming somebody (`@name …`) is a message to them: the board reaches \
                      nobody, message_agent does. A post that ends in a question is a question: \
                      nobody owes the board an answer, so ask with message_agent. A post too \
                      short to stand on its own is a remark — under about twenty-five characters \
                      is a status word, and status is set_head_name. A post you have already \
                      written word for word is refused naming the one it repeats: a memory does \
                      not need saying twice, and if what you know has changed, say what changed \
                      and reply_to the original. So are kind \"message\" (a person speaking at a \
                      dashboard) and the reserved /muxloom/ paths (muxloom's own coordination \
                      between machines). A person writing at a dashboard or from a chat app is \
                      held to none of this; an agent writing to a shared memory is.\n\n\
                      `scope` decides who inherits it: \"path\" (default) is one directory on one \
                      machine, which is where anything about a particular codebase belongs; \
                      \"machine\" is everyone on this machine, for how the machine itself \
                      behaves; \"task\" is you, whoever started you, and every subagent any of you \
                      started, for what a team learns while it works; \"global\" is every agent on \
                      every machine, so keep it for things that genuinely travel. Prefer the \
                      narrowest scope the knowledge is true in — a global board full of one \
                      repository's details is a global board nobody reads.\n\n\
                      This writes only to the talk board: it never reaches a person's chat app, \
                      and nobody on their phone is told. To reach a person where they are, use \
                      send_channel_message instead — the board and the chat app are independent \
                      surfaces and nothing routes between them."
            .into(),
        input_schema: schema(
            multi,
            json!({
                "text": { "type": "string", "description": "What to say." },
                "scope": { "type": "string", "enum": ["path", "machine", "task", "global"], "description": "Who it is for. Default path." },
                "path": { "type": "string", "description": "For scope=path: which directory. Defaults to the session's own." },
                "kind": { "type": "string", "enum": ["note"], "description": "\"note\", the default and the only kind a tool call may write: knowledge, kept and searched later — what the board is for. \"message\" is a person speaking at the dashboard and is refused here." },
                "reply_to": { "type": "string", "description": "Id of the note this corrects or adds to. One hop, and only when the correction matters to whoever reads the original later. A reply that is really an answer to somebody belongs in message_agent." },
            }),
            &["text"],
        ),
    });
    tools.push(ToolSpec {
        name: "send_channel_message",
        // No `machine` argument, on either surface: this does not reach a
        // machine, it reaches a person. Which machine dials the chat app is
        // muxloom's business, and the message says where it came from anyway.
        description: format!(
            "Reach the human who is not at a dashboard, through the chat app they bound to \
             muxloom (WeChat, or Lark). Use it when something they are waiting on is finished, \
             when you are blocked on a decision only they can make, or when a long run ends \
             while nobody is watching — and not otherwise: this arrives on somebody's phone. \
             One message per milestone, never a progress log. Write it as a summary they can \
             act on: `title` says what happened, and `text` opens with the conclusion, then the \
             numbers, then what you need from them and exactly how to answer. Keep it short: \
             `text` is capped at {text} characters and `title` at {title}, and a message over \
             either is refused rather than trimmed, because what a trim removes is whatever you \
             put last — usually the ask. If it will not fit, that is the message being too long \
             and not the cap being too small: say the conclusion and where the detail already \
             is (the board, your session, the diff). Write `text` as markdown, but keep to short \
             lines and plain lists: Lark renders headings, tables and code fences, WeChat \
             renders none of them and gets a flattened version where the words and line breaks \
             survive and the marks do not. muxloom signs it with the machine and session you \
             are in, so do not write that yourself, and never put a token, a key, or an absolute \
             home path in it. The receipt reports what you sent as `message_id`; when a reply \
             reaches you quoting one of your messages it names the quoted id, and answering with \
             that id as `reply_to` draws WeChat's quote on the same message. Their reply comes \
             back to you as a direct message: watch for it with talk_read {{ scope: \"direct\", \
             wait_seconds }}. `files` sends what you cannot describe — a plot, a screenshot, a \
             short log — from paths on the machine you are running on; a picture arrives shown in \
             the conversation and anything else as a download, after the words. Both Lark and \
             WeChat carry them, and a file that cannot be read stops the whole send rather than \
             quietly dropping it. This is a second surface, independent of the talk board: it \
             posts nothing there and reads nothing from it, so do not use talk_post to reply to a \
             person — answer them here, on the surface they wrote to.",
            text = crate::channel::READABLE_LIMIT,
            title = crate::channel::TITLE_LIMIT,
        ),
        input_schema: schema(
            false,
            json!({
                "text": { "type": "string", "description": format!("The message, as markdown. At most {} characters — a longer one is refused, not trimmed.", crate::channel::READABLE_LIMIT) },
                "title": { "type": "string", "description": format!("A few words saying what this is about, at most {} characters. Shown as the card's title; taken from a leading \"# \" line if you leave it out.", crate::channel::TITLE_LIMIT) },
                "channel": { "type": "string", "description": "Which bound channel, by id. Defaults to the one marked default, or to the only one there is." },
                "reply_to": { "type": "string", "description": "Optional platform id of a message this one answers — the message_id from an earlier send_channel_message receipt, or the id a quote-relayed message named. WeChat draws the answer as a quote of that message; other kinds ignore it." },
                "files": { "type": "array", "items": { "type": "string" }, "description": format!("Absolute paths on the machine you are running on, sent after the words as their own messages. An image ({image}) arrives shown in the conversation; anything else arrives as a download named after the file. Lark and WeChat both carry them, at most {picture} MB for a picture and {other} MB for anything else — a file over the cap or one that cannot be read stops the whole send before the words go out.", image = "png/jpg/gif/bmp/webp/tiff/ico", picture = crate::channel::cap_in_mb(crate::channel::LARK_IMAGE_BYTES), other = crate::channel::cap_in_mb(crate::channel::LARK_FILE_BYTES)) },
            }),
            &["text"],
        ),
    });
    tools.push(ToolSpec {
        name: "launch_session",
        description: format!(
            "Start a persistent codex, claude, pi, opencode, or terminal session in a working \
             directory. Use \
             this for anything long-running or interactive instead of run_shell. `resume_id` \
             resumes an agent-native conversation, or — when it names one of muxloom's own \
             session ids — revives that archived session as itself on the same number with its \
             recorded label and parent, and relaunches the children recorded under it with it; \
             `initial_prompt` seeds a fresh agent instead. The session survives this process: \
             pair every launch with a later archive \
             or delete. A session you start is recorded as yours — it shows in the dashboard \
             indented under you, and it is part of your task on the talk board — so this is how \
             you hand work to a subagent rather than losing it in a list of unrelated \
             sessions. What it may do in its own turn is yours to set: `may_message`, \
             `may_launch` and `may_reach_person` are each cut down to what you hold yourself, \
             and stay with the session through an archive and a resume.{}",
            match flavor {
                Flavor::Controller => "",
                // The daemon surface starts subagents of the agent calling it,
                // which is why `path` can be left out entirely. On another
                // machine there is no such folder to fall back to, and no
                // environment to read the caller out of either, so the path is
                // asked for and the person is asked to approve.
                Flavor::Daemon =>
                    " On this machine your own working directory is the only place you can start \
                     one: leave `path` out for it, or name somewhere inside it. On another \
                     machine, name it with `machine` and give an absolute `path` over there — the \
                     controller runs the launch once the person approves it, and the session comes \
                     back recorded as yours.",
            }
        ),
        input_schema: schema_across(
            flavor,
            json!({
                "kind": { "type": "string", "enum": ["codex", "claude", "pi", "opencode", "terminal"] },
                "path": {
                    "type": "string",
                    "description": match flavor {
                        Flavor::Controller => "Absolute working directory on the machine.",
                        Flavor::Daemon => "Absolute working directory: your own folder, or one \
                                           inside it. Defaults to your own. Required when \
                                           `machine` names another one, where you have no folder.",
                    },
                },
                "label": { "type": "string", "description": "Display name shown in the dashboard." },
                "resume_id": { "type": "string", "description": "Agent-native session id to \
                    resume, or a muxloom session id of an archived session: by muxloom id it \
                    comes back as the same session (same id, label, parent, history) and its \
                    recorded children come back with it." },
                "initial_prompt": { "type": "string", "description": "First prompt for a fresh agent." },
                "may_message": {
                    "type": "string",
                    "enum": ["parent", "task", "fleet"],
                    "description": "How far it may speak. \"parent\": back to you and nobody \
                                    else. \"task\" (the default): everyone on this piece of work \
                                    — you, its siblings, and whatever any of you start. \
                                    \"fleet\": any agent on any machine. Give it \"fleet\" when \
                                    its work is genuinely somebody else's too; leave it at \
                                    \"task\" for a helper, so half-finished work does not land \
                                    in front of agents who did not ask for it.",
                },
                "may_launch": {
                    "type": "array",
                    "items": { "type": "string", "enum": ["codex", "claude", "pi", "opencode", "terminal"] },
                    "description": "Which runtimes it may start sessions of. Defaults to your \
                                    own kind, so a team stays one kind of agent. An empty list \
                                    means it starts none and does the work itself — say that \
                                    when the work is one job rather than several.",
                },
                "may_reach_person": {
                    "type": "boolean",
                    "description": "Whether it may write to the person's chat app with \
                                    send_channel_message. Defaults to false: the person hears \
                                    about this work from you, once, rather than from every \
                                    session working on it.",
                },
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
                      Everything that session started goes with it, however many levels deep: a \
                      subagent whose master has been put down has nobody left to report to. \
                      Temporary sessions cannot be archived — delete them instead."
            .into(),
        input_schema: schema_across(
            flavor,
            json!({
                "session_id": { "type": "string", "description": "Session id from list_sessions." },
            }),
            &["session_id"],
        ),
    });
    tools.push(ToolSpec {
        name: "delete_session",
        description: "Destroy a session: needs explicit human authorization. Kills the process \
                      and deletes its history and metadata, irreversibly, and does the same to \
                      every session it started, however many levels deep. Delete only sessions \
                      you launched yourself, or ones the human named; archive_session keeps the \
                      history instead."
            .into(),
        input_schema: schema_across(
            flavor,
            json!({
                "session_id": { "type": "string", "description": "Session id from list_sessions." },
            }),
            &["session_id"],
        ),
    });
    tools.push(ToolSpec {
        name: "search_history",
        description: "Full-text search over session terminal histories, live and archived. \
                      Searches one session when session_id is given — that one is read in \
                      full, however old it is. Otherwise it searches the machine: every \
                      running session, plus the sessions written most recently, which is \
                      where a word said on this machine almost always is. When it stops \
                      short of the older ones it says so, and how many it passed over."
            .into(),
        input_schema: schema_across(
            flavor,
            json!({
                "query": { "type": "string" },
                "session_id": { "type": "string", "description": "Limit the search to one session." },
                "deep": {
                    "type": "boolean",
                    "description": "Read every capture on the machine instead of the recent \
                                    ones. Do NOT set this by default. A machine that has been \
                                    running a while holds several gigabytes of terminal \
                                    capture, and reading all of it takes seconds while every \
                                    other call to this machine waits. Search normally first; \
                                    reach for this only when that answer came back short AND \
                                    you have reason to think the thing you want was said in a \
                                    session that has been closed for a long time.",
                },
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
                "kinds": { "type": "array", "items": { "type": "string", "enum": ["codex", "claude", "pi", "opencode", "terminal"] } },
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
                      has_more_after say which way there is more. A conversation held on \
                      this machine by claude, codex or pi is read off the runtime's own \
                      transcript as it stands now (`source: transcript`); one held elsewhere, \
                      or by opencode, comes from the same backup as search_conversations and \
                      may lag a live session by a few minutes (`source: backup`)."
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
        input_schema: schema_across(
            flavor,
            json!({
                "path": { "type": "string", "description": "Absolute directory path." },
            }),
            &["path"],
        ),
    });
    tools.push(ToolSpec {
        name: "list_files",
        description: "List a directory's entries with kind, size, and mtime.".into(),
        input_schema: schema_across(
            flavor,
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
        input_schema: schema_across(
            flavor,
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
        input_schema: schema_across(
            flavor,
            json!({
                "script": { "type": "string" },
            }),
            &["script"],
        ),
    });
    // Daemon-only: only a session has its own head name. The controller is a
    // fleet-wide view with no row of its own to rename.
    if flavor == Flavor::Daemon {
        tools.push(ToolSpec {
            name: "set_head_name",
            description: "Set your own session's head name (the name on your row in the dashboard / \
                          agent list). Use it to reflect, as a whole, what you are currently working \
                          on. Keep it short (under 60 chars) and update it when the shape of your \
                          work changes, not on every step."
                .into(),
            input_schema: schema(
                false,
                json!({
                    "name": { "type": "string", "description": "Short phrase describing your current task." },
                }),
                &["name"],
            ),
        });
    }
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
///
/// A tool only belongs here if the controller will actually carry it —
/// `relay::relayed` for a look, `relay::approve_gated` for a change the person
/// signs off on first. Offering the argument on anything else would be an
/// invitation to name a machine and be answered by this one.
fn schema_across(flavor: Flavor, properties: Value, required: &[&str]) -> Value {
    let mut schema = schema(true, properties, required);
    if flavor == Flavor::Daemon {
        schema["properties"]["machine"] = json!({
            "type": "string",
            "description": "Machine id from list_machines. Defaults to this machine; another one \
                            is reached through the muxloom controller watching this machine, \
                            which has to be running. Naming another machine on a tool that \
                            changes something there puts the call to the person first, and it \
                            runs once they say so.",
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
///
/// One record is the whole question. Asking for the roster and picking a line
/// out of it made every keystroke sent to a session cost a reading of every
/// other session on the machine.
fn session_kind(session: Option<DaemonSession>) -> Option<AgentKind> {
    session.and_then(|session| session.kind.parse().ok())
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
        "machine": crate::model::machine_read_as(machine),
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

/// Find the session a wait is watching, where a wait needs it looked for.
///
/// A wait polls this every second or so for up to a minute, and what it waits
/// on is one session that is running - so that is all the rounds in the middle
/// ask for. Asking for the list carried every session on the machine, drawn
/// and classified, sixty times over, to keep one of them; asking for the
/// archive as well carried a record per conversation ever held there.
///
/// The archive is still asked once, on the round that ends the loop: nothing
/// running holds that id, so the wait is over either way, and the archive is
/// the only place that can still say what the session was. An answer that
/// could not name what it had been waiting on would be a worse answer.
fn waited_session(
    session_id: &str,
    running: impl FnOnce() -> Result<Option<DaemonSession>>,
    filed: impl FnOnce() -> Result<Vec<DaemonSession>>,
) -> Result<Option<DaemonSession>> {
    if let Some(session) = running()? {
        return Ok(Some(session));
    }
    Ok(filed()?
        .into_iter()
        .find(|session| session.id == session_id))
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

/// Which runtime this session is, when muxloom said. Used to work out what a
/// child of it may be started as when the caller does not say.
fn own_kind() -> Option<AgentKind> {
    session_env("MUXLOOM_SESSION_KIND").and_then(|kind| kind.parse().ok())
}

/// What this session was handed when it was started.
///
/// Read from the environment, which only a launch writes, for the same reason
/// the parent is: an agent asked what it is allowed to do would be answering
/// about itself. A session nobody started carries none of these variables, and
/// that is what full powers look like — a person's own agent answers to the
/// person, not to another agent.
///
/// The three go in together, so a set with one missing is a set something went
/// wrong with; each missing dial reads as the narrow answer rather than the
/// wide one, because the cost of guessing narrow is a refusal a person can
/// undo and the cost of guessing wide is a limit that was never applied.
fn own_powers() -> Powers {
    let Some(reach) = session_env("MUXLOOM_MAY_MESSAGE") else {
        return Powers::whole();
    };
    Powers {
        reach: reach.parse().unwrap_or(Reach::Task),
        launches: Powers::launches_from(&session_env("MUXLOOM_MAY_LAUNCH").unwrap_or_default()),
        may_reach_person: session_env("MUXLOOM_MAY_REACH_PERSON").as_deref() == Some("yes"),
    }
}

/// What a launch hands the session it is starting: what it asked for, cut down
/// to what the asking session actually holds.
///
/// Dials a subagent's launch says nothing about take the ordinary defaults —
/// talk within your own task, start more of the runtime you are, and leave the
/// person to the agent they asked. A launch nobody made from inside a session
/// is a person's, and what a person starts is theirs: it begins whole, and
/// only what the launch names narrows it.
fn granted_powers(arguments: &Value, own: &Powers) -> Result<Powers> {
    let mut asked = match launching_session() {
        Some(_) => Powers::default_child_of(own_kind(), own),
        None => Powers::whole(),
    };
    if let Some(reach) = optional_str(arguments, "may_message") {
        asked.reach = reach.parse().map_err(|message: String| anyhow!(message))?;
    }
    if let Some(kinds) = arguments.get("may_launch").and_then(Value::as_array) {
        asked.launches = AgentKind::ALL
            .into_iter()
            .filter(|kind| {
                kinds
                    .iter()
                    .filter_map(Value::as_str)
                    .any(|asked| asked.trim() == kind.as_str())
            })
            .collect();
    }
    if let Some(person) = arguments.get("may_reach_person").and_then(Value::as_bool) {
        asked.may_reach_person = person;
    }
    Ok(own.narrowed(&asked))
}

/// Refuse a launch this session was not given the power to make, in words that
/// say who set the limit and what to do about it. The agent reading this
/// cannot lift it — its parent can, on the next launch — so the answer is
/// where to take the question, not what flag to pass.
fn check_may_launch(own: &Powers, kind: AgentKind) -> Result<()> {
    if own.launches.contains(&kind) {
        return Ok(());
    }
    if own.launches.is_empty() {
        bail!(
            "this session may not start others: the agent that started it kept that to itself. \
             Do the work here, or ask that agent to start one for you."
        );
    }
    bail!(
        "this session may start {} sessions, and {kind} is not one of them. The agent that \
         started it set that; ask it if you need another runtime.",
        own.launches_list()
    )
}

/// How far up a chain of parents one reach check will walk. A session ten
/// handoffs down is still on the same piece of work, and past that a cycle is
/// the likelier explanation than a team.
///
/// This and the walk below are both surfaces': a moderator's session is served
/// by the controller and can have been started by another agent like anyone
/// else, so the same dial has to answer there. Only the list they walk differs
/// — one machine's records, or a target machine's out of the fleet.
const TASK_WALK_MAX: usize = 16;

/// Whether `target` is on the same piece of work as this session.
///
/// The chain of parents above the target is walked until it reaches something
/// this session recognises: the task it belongs to, or itself — anything under
/// this session is its own work by definition, and that hop is what lets a
/// subagent started on another machine be recognised from one record over
/// there. A chain that runs out unrecognised belongs to somebody else.
///
/// A process running in no session has no task to be outside of, so nothing is.
fn same_task(lineage: &[(String, Option<String>)], target: &str) -> bool {
    let mine: Vec<String> = [task_root(), launching_session()]
        .into_iter()
        .flatten()
        .collect();
    if mine.is_empty() {
        return true;
    }
    descends_from(lineage, target, &mine)
}

/// Whether `target` is one of the sessions this one started, however far down.
///
/// A session's own subtree is its own hands rather than somebody else's
/// attention, and the narrowest reach is about the second: an agent handed
/// `parent` is one that reports to whoever asked and does not go bothering the
/// rest of the fleet. If it may start a helper at all, being unable to say a
/// word to the helper leaves it holding something it cannot use — and the
/// helper, which reaches its parent by definition, asking questions upward that
/// can never be answered.
fn started_here(lineage: &[(String, Option<String>)], target: &str) -> bool {
    match launching_session() {
        Some(mine) => descends_from(lineage, target, &[mine]),
        None => false,
    }
}

/// Walk the chain of parents above `target` until it reaches one of `mine`.
///
/// A chain that runs out unrecognised, or runs longer than a team plausibly is,
/// belongs to somebody else. `mine` is never empty here: a caller with nothing
/// to compare against has already decided what that means, and the two callers
/// decide it differently.
fn descends_from(lineage: &[(String, Option<String>)], target: &str, mine: &[String]) -> bool {
    let mut at = target.trim().to_string();
    for _ in 0..TASK_WALK_MAX {
        if mine.contains(&at) {
            return true;
        }
        let Some(parent) = lineage
            .iter()
            .find(|(id, _)| *id == at)
            .and_then(|(_, parent)| parent.clone())
        else {
            return false;
        };
        at = parent;
    }
    false
}

/// Who begat whom, taken from session records.
///
/// Only the fallback for a daemon too old to answer a lineage round still reads
/// it out of whole session records, and that surface is this platform's — the
/// same reason [`lineage_of_answer`] below is spelled the same way.
#[cfg(unix)]
fn lineage(sessions: &[DaemonSession]) -> Vec<(String, Option<String>)> {
    sessions
        .iter()
        .map(|record| (record.id.clone(), record.parent.clone()))
        .collect()
}

/// The same, read back out of a rendered `list_sessions` answer — which is all
/// another machine ever hands over.
#[cfg(unix)]
fn lineage_of_answer(rendered: &str) -> Vec<(String, Option<String>)> {
    serde_json::from_str::<Vec<Value>>(rendered)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|row| {
            let id = row.get("session_id")?.as_str()?.to_string();
            Some((
                id,
                row.get("parent")
                    .and_then(Value::as_str)
                    .map(str::to_string),
            ))
        })
        .collect()
}

/// Refuse a message this session may not send, saying who it may speak to and
/// who set that, so the answer is a route rather than a flag.
fn check_may_message(
    own: &Powers,
    target: &str,
    lineage: &[(String, Option<String>)],
) -> Result<()> {
    match own.reach {
        Reach::Fleet => Ok(()),
        Reach::Task if same_task(lineage, target) => Ok(()),
        Reach::Task => bail!(
            "{target} is not on this piece of work, and this session speaks within its own: the \
             agent that started it set that. Tell that agent what needs saying, and let it carry \
             the message."
        ),
        // Upward to the one agent that asked, and downward into whatever this
        // session started to answer it with. Everything else on the machine is
        // somebody else's, which is the whole of what this dial says.
        Reach::Parent
            if session_env("MUXLOOM_SESSION_PARENT").as_deref() == Some(target.trim())
                || started_here(lineage, target) =>
        {
            Ok(())
        }
        Reach::Parent => bail!(
            "this session answers to the agent that started it, and to the sessions it started \
             itself, and to nobody else — which is what that agent asked for. Report to it, and \
             it can pass anything on."
        ),
    }
}

/// Whether the same weighing comes out yes with no records at all.
///
/// Most of what it is asked needs none: a session speaking to the agent that
/// started it, or to the task it belongs to, or a process running in no session
/// and so outside nobody's. Fetching the records to answer those is a round
/// trip to a daemon that hands back every conversation the machine has ever
/// held — and across a relay, a round trip to another machine, before a
/// keystroke goes anywhere.
///
/// Asking with nothing in hand can only be stricter than asking with them. The
/// walk in [`descends_from`] recognises the target on its first hop, before it
/// consults anything, and otherwise follows only parents it was given: records
/// can add a path to the answer and never take one away. So a yes here is a yes
/// there, and a no here settles nothing and goes on to ask properly.
fn reaches_without_records(own: &Powers, target: &str) -> bool {
    check_may_message(own, target, &[]).is_ok()
}

/// The session a call would put words into, when it is one of the calls that
/// does.
///
/// `message_agent` is not the only door into another agent's prompt box.
/// `send_input` types straight into it, without even the envelope that says who
/// is speaking, and a trigger types into it later on a pattern. All three are
/// this session speaking to that one, so one dial has to answer for all three —
/// a reach enforced on the politest of them and nowhere else is a fence with a
/// gate beside it.
///
/// A call that names no session is left alone: the tool itself refuses it a
/// moment later, and with a better sentence than this could manage.
fn written_to<'a>(name: &str, arguments: &'a Value) -> Option<&'a str> {
    let writes = match name {
        "message_agent" | "send_input" => true,
        // Only the kind that types. A `notify` trigger raises a flag on the
        // session for a person to see, and says nothing to the agent in it.
        "trigger" => {
            optional_str(arguments, "action") == Some("set")
                && optional_str(arguments, "action_kind") == Some("send_input")
        }
        _ => false,
    };
    writes
        .then(|| optional_str(arguments, "session_id"))
        .flatten()
}

/// Refuse a message to the person from a session that was not handed the
/// person to talk to. One agent answers them, and it is not this one.
fn check_may_reach_person(own: &Powers) -> Result<()> {
    if own.may_reach_person {
        return Ok(());
    }
    bail!(
        "this session does not write to the person's chat: the agent that started it is the one \
         answering them, and asked to keep it that way. Tell that agent what the person needs to \
         hear, and it will carry it."
    )
}

/// Refuse a board post aimed wider than this session may speak.
///
/// The board is a set of rooms rather than one, and the same dial says which
/// of them a session may be heard in: its own task's room always, the folder
/// it works in once it may talk to the others working there, and the machine
/// and the world only when it may speak to any agent at all.
fn check_may_post(own: &Powers, scope: &TalkScope) -> Result<()> {
    let allowed = matches!(
        (own.reach, scope),
        (Reach::Fleet, _) | (_, TalkScope::Task { .. }) | (Reach::Task, TalkScope::Path { .. })
    );
    if allowed {
        return Ok(());
    }
    match own.reach {
        Reach::Parent => bail!(
            "this session posts to its own task's board and nowhere else: the agent that started \
             it set that. Use scope \"task\", or report to that agent."
        ),
        _ => bail!(
            "this session speaks within its own task and the folder it works in: the agent that \
             started it set that. Use scope \"task\" or \"path\", or ask that agent to say it \
             more widely."
        ),
    }
}

/// The caller a relayed launch names for itself. The controller's own process
/// never runs inside a session, so a launch arriving through a relay has no
/// environment to read and would otherwise lose its parent entirely: the
/// relay runner copies the submitting session's id into the arguments, and it
/// is consulted only when the environment says nothing. The daemon flavor
/// never reads it - an agent on its own machine already has the environment,
/// and trusting an argument there would let any caller name a parent it likes.
fn relayed_caller(arguments: &Value) -> Option<String> {
    launching_session().or_else(|| {
        optional_str(arguments, "_muxloom_caller")
            .map(str::trim)
            .filter(|id| crate::runtime::is_managed_session_id(id))
            .map(str::to_string)
    })
}

/// Where a relayed launch carries the grant it was already cut down to.
const POWERS_ARGUMENT: &str = "_muxloom_powers";

/// Write the grant into a call about to leave for the controller.
///
/// A launch aimed at another machine is still this session's launch, and what
/// it may hand on is written in this session's environment — which the
/// controller cannot read, because the controller runs in no session at all.
/// So the side that can read it works the grant out and sends it along.
#[cfg(unix)]
fn stamp_powers(arguments: &mut Value, powers: &Powers) {
    if let (Some(object), Ok(value)) = (arguments.as_object_mut(), serde_json::to_value(powers)) {
        object.insert(POWERS_ARGUMENT.into(), value);
    }
}

/// The grant a relayed launch arrived carrying, if it did.
///
/// Trusted on the same terms as `relayed_caller`: only where there is no
/// environment to read instead. On a machine where an agent could write this
/// itself, its own environment is what answers, and this is never consulted.
fn relayed_powers(arguments: &Value) -> Option<Powers> {
    if session_env("MUXLOOM_SESSION_ID").is_some() {
        return None;
    }
    arguments
        .get(POWERS_ARGUMENT)
        .cloned()
        .and_then(|value| serde_json::from_value(value).ok())
}

/// The deepest child chain one fleet resume walks. A coordinator five
/// handoffs from its master is already a team; below that the sessions have
/// their own coordinators, and it is their resumes that walk their fleets.
const FLEET_RESUME_MAX_DEPTH: usize = 5;
/// The most children one fleet resume will relaunch. A runaway tree becomes
/// a truncated line in the master's caption rather than a stampede of
/// launches.
const FLEET_RESUME_MAX_SESSIONS: usize = 32;

/// The conversation a stored session would resume as: the thread the daemon
/// had matched it to while it ran, or - never matched - the conversation its
/// launch was told to reopen.
pub(crate) fn native_resume_id(session: &DaemonSession) -> Option<&str> {
    session
        .thread
        .as_deref()
        .or(session.seed.as_deref())
        .filter(|id| !id.trim().is_empty())
}

/// What a fleet resume does with one child.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FleetMemberAction {
    /// Dead or archived: it comes back on its own record, same id and label,
    /// native conversation and all if the runtime can reopen one.
    Relaunch,
    /// Still running under its master: nobody relaunches a live agent; the
    /// master is only told it exists.
    Running,
    /// A temporary scratch chat: it left nothing behind to resume.
    Ephemeral,
}

#[derive(Debug, Clone)]
pub(crate) struct FleetMember {
    pub(crate) record: DaemonSession,
    pub(crate) native: Option<String>,
    pub(crate) action: FleetMemberAction,
}

/// Everything still hanging off `root_id` by its recorded parent link,
/// shallowest first, within the walk's depth and total bounds. The parent
/// link is the only account of a fleet that survives a split, which is
/// exactly why the walk reads nothing else. Live children are reported and
/// not restarted; temporary ones are only reported.
pub(crate) fn fleet_resume_plan(
    sessions: &[DaemonSession],
    root_id: &str,
) -> (Vec<FleetMember>, bool) {
    let mut members = Vec::new();
    let mut truncated = false;
    let mut seen: BTreeSet<&str> = [root_id].into_iter().collect();
    let mut frontier: Vec<&str> = vec![root_id];
    let mut depth = 0usize;
    while !frontier.is_empty() {
        depth += 1;
        let mut level: Vec<&DaemonSession> = sessions
            .iter()
            .filter(|child| {
                child
                    .parent
                    .as_deref()
                    .is_some_and(|parent| frontier.contains(&parent))
                    && !seen.contains(child.id.as_str())
            })
            .collect();
        level.sort_by_key(|child| (child.created_at, child.id.as_str()));
        let mut next: Vec<&str> = Vec::new();
        for child in level {
            if members.len() >= FLEET_RESUME_MAX_SESSIONS {
                truncated = true;
                break;
            }
            seen.insert(child.id.as_str());
            let action = if child.temporary {
                FleetMemberAction::Ephemeral
            } else if child.dead || child.archived {
                FleetMemberAction::Relaunch
            } else {
                FleetMemberAction::Running
            };
            next.push(child.id.as_str());
            members.push(FleetMember {
                record: child.clone(),
                native: native_resume_id(child).map(str::to_string),
                action,
            });
        }
        if truncated {
            break;
        }
        if depth >= FLEET_RESUME_MAX_DEPTH {
            truncated = sessions.iter().any(|child| {
                child
                    .parent
                    .as_deref()
                    .is_some_and(|parent| next.contains(&parent))
                    && !seen.contains(child.id.as_str())
            });
            break;
        }
        frontier = next;
    }
    (members, truncated)
}

/// What became of one child during a fleet resume.
#[derive(Debug, Clone)]
pub(crate) struct FleetOutcome {
    pub(crate) record: DaemonSession,
    /// "restored" | "fresh" | "running" | "temporary" | "out_of_scope" |
    /// "unresumed"
    pub(crate) status: &'static str,
    /// The native conversation the child came back with, when it did.
    pub(crate) resumed_with: Option<String>,
    /// Why it did not, when it did not.
    pub(crate) detail: Option<String>,
}

pub(crate) fn fleet_outcome_json(outcomes: &[FleetOutcome]) -> Vec<Value> {
    outcomes
        .iter()
        .map(|outcome| {
            json!({
                "old_session_id": outcome.record.id,
                "label": outcome.record.label,
                "kind": outcome.record.kind,
                "native": native_resume_id(&outcome.record),
                "status": outcome.status,
                "resumed_with": outcome.resumed_with,
                "detail": outcome.detail,
                "recap": outcome.record.recap,
            })
        })
        .collect()
}

/// The caption a resumed master's first turn carries: the machine-readable
/// list of its children with what muxloom could do for each, and the exact
/// calls that can still reach the ones it could not. A master must never come
/// back ignorant of its fleet again - that silence is what let five children
/// go unread for a day.
fn fleet_resume_caption(
    master_id: &str,
    master_label: &str,
    outcomes: &[FleetOutcome],
    truncated: bool,
) -> String {
    let count = |wanted: &str| outcomes.iter().filter(|o| o.status == wanted).count();
    let name = if master_label.trim().is_empty() {
        master_id
    } else {
        master_label
    };
    let mut lines = vec![format!(
        "[muxloom] Fleet resume report for {name} ({master_id}): {} children found by parent \
         links - restored {}, fresh {}, still running {}, unresumed {}, temporary {}{}.",
        outcomes.len(),
        count("restored"),
        count("fresh"),
        count("running"),
        count("unresumed"),
        count("temporary"),
        if truncated {
            ", walk truncated at the cap"
        } else {
            ""
        },
    )];
    for outcome in outcomes {
        let reachable = outcome.status != "unresumed" && outcome.status != "out_of_scope";
        lines.push(format!(
            "child old={} label=\"{}\" kind={} native={} status={} session={} recap=\"{}\"",
            outcome.record.id,
            outcome.record.label,
            outcome.record.kind,
            native_resume_id(&outcome.record).unwrap_or("none"),
            outcome.status,
            if reachable {
                outcome.record.id.as_str()
            } else {
                "-"
            },
            outcome.record.recap.as_deref().unwrap_or("-"),
        ));
        if !reachable {
            lines.push(format!(
                "  resume with: launch_session {{\"kind\": \"{}\", \"resume_id\": \"{}\", \"path\": \
                 \"{}\", \"label\": \"{}\"}}",
                outcome.record.kind,
                native_resume_id(&outcome.record).unwrap_or(&outcome.record.id),
                outcome.record.path,
                outcome.record.label,
            ));
        }
    }
    lines.push(
        "Children are found by their recorded parent link; muxloom keeps that link pointed at \
         the id that answers, so it survived your absence."
            .into(),
    );
    lines.join("\n")
}

/// What a fleet resume has already put back, for the one case where saying
/// so is the whole message: the master failed to come back, and the children
/// that did are now running under a number that answers nothing. A caller
/// told only that the resume failed would go looking for a fleet it cannot
/// see, or start a second one on top of it.
fn fleet_already_back(master_id: &str, outcomes: &[FleetOutcome]) -> Option<String> {
    let back: Vec<&str> = outcomes
        .iter()
        .filter(|outcome| outcome.status == "restored" || outcome.status == "fresh")
        .map(|outcome| outcome.record.id.as_str())
        .collect();
    (!back.is_empty()).then(|| {
        format!(
            "{} child session(s) came back before this failed and are running under \
             {master_id}, which did not: {}. Resuming {master_id} again picks them up where \
             they are rather than starting them twice.",
            back.len(),
            back.join(", ")
        )
    })
}

/// What a `launch_session` resume_id means when it names a muxloom session:
/// `Ok(None)` passes it on as the ordinary relaunch of an agent-native
/// conversation; `Ok(Some(record))` is an archived session coming back as
/// itself. An error is the explicit refusal - a live session still holds that
/// number, and one conversation never gets a second shadow identity - or a
/// number this machine has never heard of.
fn fleet_resume_target<'a>(
    sessions: &'a [DaemonSession],
    resume_id: &str,
) -> Result<Option<&'a DaemonSession>> {
    if !crate::runtime::is_daemon_session_id(resume_id) {
        return Ok(None);
    }
    let Some(record) = sessions.iter().find(|session| session.id == resume_id) else {
        bail!(
            "no session {resume_id} on this machine to resume; launch_session's resume_id is \
             either a muxloom session id recorded here or an agent-native conversation id"
        );
    };
    if record.temporary {
        bail!("{resume_id} is a temporary scratch chat; it left nothing to resume");
    }
    if !record.dead && !record.archived {
        bail!(
            "session {resume_id} is still live; refusing to resume over a running session - \
             send it a message instead"
        );
    }
    Ok(Some(record))
}

/// The first-turn prompt for a resumed child with no native conversation to
/// reopen: it wakes knowing what it was and where the rest of it lives.
fn synthetic_child_prompt(record: &DaemonSession) -> String {
    let name = if record.label.trim().is_empty() {
        record.id.as_str()
    } else {
        record.label.as_str()
    };
    format!("You are resumed as \"{name}\" without your old context; ask the coordinator.")
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
///
/// The surface that consults it is the daemon's, which exists only where the
/// daemon does; the rule it states is worth testing everywhere all the same.
#[cfg_attr(not(unix), allow(dead_code))]
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

/// What the calling session is called *now*, asked of whoever can see the
/// session table.
///
/// `MUXLOOM_SESSION_LABEL` is stamped into the keeper's environment once, when
/// the session is launched, and no one can change a running process's mind
/// about it afterwards. Every name the session has acquired since is invisible
/// from in here: the one it gave itself with `set_head_name`, the one a person
/// typed over it in the dashboard, the title its runtime wrote for the
/// conversation. So a name read off the environment is what the session was
/// called at birth — which, for the many launched with no name at all, is
/// nothing — and that is the name a person was reading on their phone.
///
/// Asked rather than inferred, and only of the session's own record: an agent
/// naming itself in an argument could only get it wrong.
///
/// One record is the whole question, and the session it is about is the one
/// asking — running, by definition. So `look` is handed the id: every message
/// an agent sends used to have the machine gather, draw and classify every
/// session on it, and read its own line off the bottom of that.
fn session_name_now(look: impl FnOnce(&str) -> Result<Option<DaemonSession>>) -> Option<String> {
    let id = session_env("MUXLOOM_SESSION_ID")?;
    let session = look(&id).ok()??;
    let named = |value: &str| {
        let value = value.trim();
        (!value.is_empty()).then(|| value.to_string())
    };
    named(&session.label).or_else(|| session.title.as_deref().and_then(named))
}

/// Who a channel message says it is from, as the human should read it: what the
/// session is called if it is called anything, and the folder it runs in if it
/// is not — a session named is somebody, a session left at its defaults is
/// where the work is. A machine label is added when there is one, so two
/// machines talking into the same chat stay tellable apart.
///
/// `now` is what the session table says it is called this minute, from
/// `session_name_now`; the environment is the fallback for a lookup that could
/// not be made. The session id is never one of the answers. It identifies the
/// session perfectly and names it not at all, and a person reading their phone
/// cannot do anything with `muxloomd-claude-1787996682-39374-2` except squint
/// at it — the runtime and the machine at least say which agent this is.
fn speaker(now: Option<String>) -> String {
    let name = now
        .or_else(|| session_env("MUXLOOM_SESSION_LABEL"))
        .or_else(|| {
            session_env("MUXLOOM_SESSION_PATH")
                .filter(|path| !path.trim().is_empty())
                .as_deref()
                .and_then(folder_name)
        })
        .or_else(|| session_env("MUXLOOM_SESSION_KIND"))
        .unwrap_or_default();
    let mut parts = vec![name];
    if let Some(machine) = session_env("MUXLOOM_MACHINE_LABEL")
        .or_else(|| session_env("MUXLOOM_MACHINE"))
        .filter(|machine| !machine.trim().is_empty())
    {
        parts.push(machine);
    }
    parts
        .into_iter()
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(" · ")
}

/// The last path component of a working directory, the name a session falls
/// back on when nobody has given it one.
fn folder_name(path: &str) -> Option<String> {
    std::path::Path::new(path.trim_end_matches(['/', '\\']))
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .filter(|name| !name.is_empty() && name != "/")
}

/// Which message this one answers, if the caller named one.
///
/// Blank is "none". An id the chat never saw travels through untouched: the
/// phone then shows the message unquoted, and losing the message over a
/// decoration is the worse failure.
fn channel_reply_to(arguments: &Value) -> Option<&str> {
    optional_str(arguments, "reply_to")
        .map(str::trim)
        .filter(|id| !id.is_empty())
}

/// Post one channel message and report what became of it.
///
/// Shared by both surfaces. Which machine dials the chat app differs — its own,
/// for whoever is standing on it — but what the human receives must not, and a
/// message that renders differently depending on who sent it is a message they
/// have to work out rather than read.
fn send_channel(
    binding: &crate::channel::ChannelBinding,
    arguments: &Value,
    environment: &[(String, String)],
    now: Option<String>,
    leave: impl FnOnce(crate::channel::ChannelReceipt),
) -> Result<String> {
    check_may_reach_person(&own_powers())?;
    let message = crate::channel::Outgoing {
        title: optional_str(arguments, "title").unwrap_or_default().into(),
        text: required_str(arguments, "text")?.into(),
        signature: speaker(now.clone()),
        files: string_list(arguments, "files")?
            .into_iter()
            .map(Into::into)
            .collect(),
    };
    // Before anything is dialled, so a refused message costs nothing and the
    // agent gets the error while it still has the text in hand.
    crate::channel::refuse_if_too_long(&message)?;
    let sent =
        crate::channel::send_reply(binding, &message, channel_reply_to(arguments), environment)?;
    // WeChat answers a send it accepted and then dropped exactly as it answers
    // one it delivered — code 0, HTTP 200 — except that it issues no id of its
    // own, which is what a stale conversation token looks like from here. The
    // verdict has always known the difference; from here on so does everything
    // built on top of it, because a message the person never saw must not be
    // recorded or described as one they did.
    let delivered = sent.delivered();
    // A receipt is what turns the human's reply into an answer: without one it
    // lands on the board, which is not wrong but is not this conversation. It
    // needs a session to name, so a call from outside a session leaves none —
    // and so does a send that was never delivered. Such a receipt could never be
    // matched by a quote anyway, since the id in it is one this side minted and
    // no reply will ever name; all it could still do is make this session the
    // one the person spoke to last, and aim their next unaddressed word at an
    // agent whose message they never read.
    if let (true, false, Some(session_id)) = (
        delivered,
        sent.message_id.is_empty(),
        session_env("MUXLOOM_SESSION_ID"),
    ) {
        leave(crate::channel::ChannelReceipt {
            channel: sent.channel.clone(),
            message_id: sent.message_id.clone(),
            machine: session_env("MUXLOOM_MACHINE").unwrap_or_else(|| "local".into()),
            session_id,
            // The name the chat will show against their reply, and the one
            // `/list` reads back: the live one, for the same reason the
            // signature is.
            label: now
                .or_else(|| session_env("MUXLOOM_SESSION_LABEL"))
                .unwrap_or_default(),
        });
    }
    let wechat_json = sent.wechat.as_ref().map(|v| {
        json!({
            "code": v.code,
            "reason": v.reason,
            "delivery_confirmed": v.delivery_confirmed,
        })
    });
    Ok(pretty(&json!({
        "channel": sent.channel,
        "through": sent.through,
        "message_id": sent.message_id,
        "signed": message.signature,
        "files": sent.files,
        "wechat": wechat_json,
        "note": match delivered {
            true => "Sent to a person, not to an agent. Their answer comes back as a direct \
                     message: talk_read { scope: \"direct\", wait_seconds }.",
            // Promising an answer here is the one thing this result must not
            // do: an agent told its message landed will sit waiting on a reply
            // to something nobody received, which is a worse failure than being
            // told plainly that the send went nowhere.
            false => "NOT DELIVERED. WeChat accepted this and issued no id of its own, which is \
                      what a message dropped on a stale conversation token looks like — assume \
                      the person did not see it and do not wait for an answer. Nothing this side \
                      can do repairs it: only an inbound message renews the token, so the \
                      conversation opens again when they say anything at all to the bot. Sending \
                      it again before then lands in the same place.",
        },
    })))
}

fn session_voice(now: Option<String>) -> TalkVoice {
    TalkVoice {
        session_id: session_env("MUXLOOM_SESSION_ID"),
        // What this session is called now, not what it was named at launch:
        // this is how every other agent reads who just spoke to them, and a
        // board full of agents under their birth names is a board nobody can
        // navigate. See `session_name_now`.
        label: now.or_else(|| session_env("MUXLOOM_SESSION_LABEL")),
        kind: session_env("MUXLOOM_SESSION_KIND"),
        // Speaking as a person is the dashboard's privilege; anything reaching
        // the board through a tool call is an agent, whoever asked for it.
        human: false,
        channel: None,
        channel_quote: None,
    }
}

/// The post a `talk_post` call describes. The scope's machine is left empty on
/// purpose: the daemon that mints the message is the one that knows its own
/// origin key, and a caller naming someone else's machine would be filing
/// under a board it does not own.
///
/// The author is passed in rather than defaulted. `Talk::post` fills in the
/// machine and nothing else, so a draft that named nobody was filed as nobody —
/// every board post read back as `"name": "someone"` with no session and no
/// kind, which is a board you cannot tell two agents apart on.
fn talk_draft(arguments: &Value, author: TalkAuthor) -> Result<TalkDraft> {
    let text = required_str(arguments, "text")?;
    if text.len() > MAX_TEXT {
        bail!(
            "text must be shorter than {MAX_TEXT} bytes: post what the others need to know, not \
             the transcript"
        );
    }
    // A post nobody labelled is a note. The board is a memory: what an agent
    // writes on it is meant to outlive the conversation that wrote it, and
    // "message" survives only for a person typing at the dashboard and for
    // older machines whose posts still arrive labelled that way.
    let kind = TalkKind::parse(optional_str(arguments, "kind").unwrap_or("note"))?;
    if kind == TalkKind::Direct {
        bail!(
            "talk_post writes to a board, and a direct message goes to one session: use \
             message_agent instead"
        );
    }
    // Saying "the board is a memory" and then taking anything that was handed
    // to it is how a board fills with a fleet's passing remarks. A note is
    // what is written down; the other kind is a person speaking at the
    // dashboard, and a tool call is never a person.
    if kind == TalkKind::Message {
        bail!(
            "the board is a memory, and \"message\" is a person speaking at the dashboard. \
             Write it down as a note if it will still be worth knowing tomorrow; say it to \
             somebody with message_agent if it will not"
        );
    }
    let scope = match optional_str(arguments, "scope").unwrap_or("path") {
        "global" => TalkScope::Global,
        "machine" => TalkScope::Machine {
            machine: String::new(),
        },
        "path" => {
            let path = optional_str(arguments, "path")
                .map(str::to_string)
                .or_else(|| session_env("MUXLOOM_SESSION_PATH"))
                .context(
                    "scope \"path\" needs a path, and this process is not running inside a \
                     muxloom session that could name one",
                )?;
            // muxloom keeps its own coordination notes under this namespace —
            // which machine holds a chat account, what a person said about an
            // approval parked elsewhere. They are on the board because every
            // machine replicates it, and they are nobody's memory.
            if path.starts_with(crate::talk::WIRE_PATH_PREFIX) {
                bail!(
                    "{} is muxloom's own, for coordination between machines rather than for \
                     anything anybody wrote down. Post under the directory the knowledge is \
                     about",
                    crate::talk::WIRE_PATH_PREFIX
                );
            }
            TalkScope::Path {
                machine: String::new(),
                path,
            }
        }
        "task" => TalkScope::Task {
            machine: String::new(),
            root_session: task_root().context(
                "scope \"task\" is the agents working on one piece of work, and this process is \
                 not running inside a muxloom session that could say which one",
            )?,
        },
        other => bail!("unknown scope {other}: use path, machine, task, or global"),
    };
    check_may_post(&own_powers(), &scope)?;
    Ok(TalkDraft {
        scope,
        author,
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

/// Who a message is from, board post or direct alike.
///
/// The machine matters most for a direct message, which is filed on the
/// *target's* board: a sender that says nothing about where it is would be
/// recorded as speaking from the machine it reached. A board post crossing to
/// another machine's board has the same problem, and one filed here would have
/// been backfilled correctly anyway — so both ask.
fn message_author(now: Option<String>, local: impl FnOnce() -> Result<TalkState>) -> TalkAuthor {
    let mut author = TalkAuthor {
        machine: session_env("MUXLOOM_MACHINE").unwrap_or_default(),
        machine_label: session_env("MUXLOOM_MACHINE_LABEL").unwrap_or_default(),
        voice: session_voice(now),
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
            // The way back to a person who wrote in from a chat app. An agent
            // reading its own board afterwards — catching up on what it was
            // asked while it was busy — needs the same return address the
            // delivered envelope carried, or the answer to a question read
            // here has nowhere to go.
            "channel": &message.author.voice.channel,
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
    holder: impl FnMut(&str) -> Option<DaemonSession>,
) -> Result<String> {
    let wait =
        Duration::from_secs(optional_u64(arguments, "wait_seconds", 0).min(TALK_MAX_WAIT_SECONDS));
    let started = Instant::now();
    loop {
        let mut page = read(&filter)?;
        let elapsed = started.elapsed();
        if !page.messages.is_empty() || elapsed >= wait {
            // A wait that ends empty is the one that needs explaining. Say
            // which of the caller's own messages are still unanswered and what
            // the sessions holding them are doing, so the next move is a fact
            // rather than a guess.
            let outstanding = if page.messages.is_empty() && !wait.is_zero() {
                unanswered(&filter, &mut read, holder)
            } else {
                Vec::new()
            };
            // Weighed only now that the page is going out, never before the
            // decision above: a wait whose page were emptied here would read
            // as nothing-was-said and go round again, waiting out its whole
            // timeout on messages it had already been handed.
            let matched = page.messages.len();
            // A caller following a cursor is handed the oldest end of what is
            // new, so the end to give up here is the other one - and the cursor
            // has to come back with it. Trimming the far end and handing back a
            // cursor that reached past it would lose exactly what the page
            // ordering was changed to stop losing.
            let keep = if filter.since.is_empty() {
                Keep::Newest
            } else {
                Keep::Oldest
            };
            let held = fit_within(&mut page.messages, TALK_MAX_RESPONSE_CHARS, keep);
            let overflowed = held < matched;
            if overflowed && keep == Keep::Oldest {
                page.cursor = encode_cursor(&cursor_through(&page.messages, &filter.since));
            }
            let messages: Vec<Value> = page.messages.iter().map(talk_json).collect();
            return Ok(pretty(&json!({
                "messages": messages,
                "cursor": page.cursor,
                "truncated": page.truncated || overflowed,
                "waited_ms": elapsed.as_millis() as u64,
                "waiting_on": (!outstanding.is_empty()).then_some(&outstanding),
                "note": if overflowed && keep == Keep::Oldest {
                    Some(format!(
                        "too much matched to hand back at once: {held} of {matched} messages \
                         shown, oldest kept. The cursor stops where they stop, so read again \
                         with it to be given the rest, or narrow the read with `query`, \
                         `scope`, or a smaller `limit`."
                    ))
                } else if overflowed {
                    Some(format!(
                        "too much matched to hand back at once, so the oldest was left out: \
                         {held} of {matched} messages shown, newest kept. Read again with \
                         `before` set to the oldest ts you got to page further back, or \
                         narrow the read with `query`, `scope`, or a smaller `limit`."
                    ))
                } else if page.truncated {
                    Some(
                        "more messages matched than fit: read again with the cursor to be \
                         given the rest, or with `before` set to the oldest ts you got to \
                         page further back"
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

/// Which end of a page survives a page that weighs too much.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Keep {
    /// Drop the oldest. What someone arriving at the board wants: catching up
    /// is what the board is read for, and `before` pages back to the rest.
    Newest,
    /// Drop the newest. What someone following a cursor needs, so the run they
    /// are given stays contiguous with the one before it and the cursor can be
    /// pulled back to the end of it.
    Oldest,
}

/// Drop whole messages off one end of a page until what is left fits `budget`
/// characters,
/// and say how many were kept.
///
/// Whole messages, never a cut through the middle of one: half a handover
/// reads as a complete one that ends oddly, and an agent acting on it has no
/// way to tell. The last message stays whatever it weighs — a page emptied to
/// respect a ceiling answers a question nobody asked.
fn fit_within(messages: &mut Vec<TalkMessage>, budget: usize, keep: Keep) -> usize {
    let weights: Vec<usize> = messages
        .iter()
        .map(|message| talk_json(message).to_string().chars().count())
        .collect();
    let mut total: usize = weights.iter().sum();
    let mut cut = 0;
    while cut + 1 < messages.len() && total > budget {
        total -= match keep {
            Keep::Newest => weights[cut],
            Keep::Oldest => weights[messages.len() - 1 - cut],
        };
        cut += 1;
    }
    match keep {
        Keep::Newest => messages.drain(..cut),
        Keep::Oldest => messages.drain(messages.len() - cut..),
    };
    messages.len()
}

/// How far a cursor may reach when it describes a run of messages rather than
/// the whole board: to the newest of each origin actually in hand, and no
/// further, with an origin the run says nothing about left where the caller
/// already was.
fn cursor_through(messages: &[TalkMessage], floor: &TalkVector) -> TalkVector {
    let mut vector = floor.clone();
    for message in messages {
        let mark = vector.entry(message.origin.clone()).or_default();
        *mark = (*mark).max(message.seq);
    }
    vector
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
    mut holder: impl FnMut(&str) -> Option<DaemonSession>,
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
    let now = now_ms();
    waiting
        .into_iter()
        .map(|message| {
            let to = message.to.as_ref().expect("filtered to addressed messages");
            // Asked for by name, one at a time. There is rarely more than a
            // message or two outstanding, and the list these used to be found
            // in carries every conversation the machine has ever held, each
            // running one drawn and classified to answer - fetched at the end
            // of every wait that came back empty, which is most of them.
            let session = holder(&to.session_id);
            let session = session.as_ref();
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
        "machine": crate::model::machine_read_as(machine),
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

/// What `read_screen` answers with: the page as text, and a trailer saying
/// what the page is — how many rows it holds, where it ends, and whether and
/// where there is more above it.
///
/// `rows` counts the rows the page holds before blank ones are folded away,
/// which is what the next page has to be offset by; the trailer says that
/// offset outright so a reader paging back does no arithmetic. A full-screen
/// program is named as one, because the terminal keeps nothing of what it
/// draws: paging back through its history reaches only what ran before it
/// opened, and a reader who wants its earlier turns wants its transcript.
fn screen_page(read: &ScreenRead, raw: bool) -> String {
    let plain = plain_screen(&read.text);
    let body = if raw { plain } else { read_result(&plain) };
    let rows = match read.text.is_empty() {
        true => 0,
        false => read.text.lines().count(),
    };
    let offset_from_bottom = read.offset_from_bottom;
    let older = !read.reached_start || offset_from_bottom + rows < read.total_rows;
    let mut trailer =
        format!("[rows={rows} offset_from_bottom={offset_from_bottom} older_history_above={older}");
    if older {
        trailer.push_str(&format!(
            " next_offset_from_bottom={}",
            offset_from_bottom + rows
        ));
    }
    trailer.push(']');
    if read.alternate_screen {
        trailer.push_str(
            "\n[full-screen program: this is its whole screen, and the terminal keeps no \
             history of what it drew before. Its earlier turns are in its own transcript — \
             search_conversations / read_conversation.]",
        );
    }
    format!("{body}\n\n{trailer}")
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
        if bytes[index..].starts_with(WRAPPED_ROW_MARK_TEXT.as_bytes()) {
            plain.extend_from_slice(WRAPPED_ROW_MARK_TEXT.as_bytes());
            index += WRAPPED_ROW_MARK_TEXT.len();
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
    // A row that ran off the right edge carried on in the row under it, and
    // the renderer marks it so: the two are one line as the program wrote it,
    // and a reader wants the line, not the edge of the pane it was cut at.
    let mut lines: Vec<String> = Vec::new();
    let mut joining = false;
    for line in plain.split('\n') {
        let (line, wraps) = match line.strip_suffix(WRAPPED_ROW_MARK_TEXT) {
            Some(line) => (line, true),
            None => (line, false),
        };
        match lines.last_mut() {
            Some(previous) if joining => previous.push_str(line),
            _ => lines.push(line.to_string()),
        }
        joining = wraps;
    }
    for line in &mut lines {
        let trimmed = line.trim_end().len();
        line.truncate(trimmed);
    }
    while lines.last().is_some_and(|line| line.is_empty()) {
        lines.pop();
    }
    lines.join("\n")
}

/// [`crate::terminal_session::WRAPPED_ROW_MARK`] as it survives
/// [`plain_screen`]'s pass over the escapes, which strips string sequences
/// only when they open with ESC; this one is left whole for the join above
/// to find.
const WRAPPED_ROW_MARK_TEXT: &str = "\x1b_wrap\x1b\\";

/// What `read_screen` returns by default: the screen as a person reads it.
/// Border glyphs and the indent they carry are off each row, a row that is
/// only border or blank is gone, the status bar pinned to the bottom is
/// screened out, and each surviving row is one run of words in order.
fn read_result(plain: &str) -> String {
    let mut rows: Vec<String> = Vec::new();
    for line in plain.lines() {
        let row = line
            .trim_start_matches(chrome_char)
            .trim_end_matches(chrome_char);
        let row = row.split_whitespace().collect::<Vec<_>>().join(" ");
        if !row.is_empty() {
            rows.push(row);
        }
    }
    // The status bar is pinned to the bottom, and a tip or a line of content
    // may sit between two of its rows, so the last few rows are screened
    // rather than popped from the end: a bar row goes wherever it is, the
    // rest stays put.
    let scan = rows.len().min(4);
    for row in rows.iter_mut().rev().take(scan) {
        if is_status_bar(row) {
            *row = String::new();
        }
    }
    rows.retain(|row| !row.is_empty());
    rows.join("\n")
}

/// A character a frame, rather than the content, is made of: a box-drawing
/// or block-element glyph, or the whitespace a border indents its row with.
/// Both ends of a row are stripped of them, so a `│ text │` row reads `text`.
fn chrome_char(c: char) -> bool {
    c.is_ascii_whitespace()
        || ('\u{2500}'..='\u{257F}').contains(&c)
        || ('\u{2580}'..='\u{259F}').contains(&c)
}

/// The persistent status/footer bar a TUI pins to the bottom. It is screened
/// only from the last few rows, where that bar lives, so a line of real
/// content is safe. A row is a bar when it carries a strong signal — a
/// braille spinner or a usage figure — or when it is a build footer (ends on
/// a version stamp next to a path or an mcp marker), a mode bar (Claude
/// Code's `⏵⏵ auto mode on (shift+tab to cycle) · esc to interrupt`), a
/// key-hint bar, or the bare glyph of an empty prompt. The version signal
/// alone is too common in prose ("shipped in release 1.2.3"), so it only
/// counts beside a path or an mcp marker; the same restraint keeps a sentence
/// that merely mentions a key alive — a key hint is a short row, and a mode
/// bar names the key that cycles it.
fn is_status_bar(row: &str) -> bool {
    let lower = row.to_ascii_lowercase();
    if row.chars().any(|c| ('\u{2800}'..='\u{28FF}').contains(&c)) || usage_figure(&lower) {
        return true;
    }
    if ends_with_version(&lower) && (lower.contains('/') || lower.contains("mcp")) {
        return true;
    }
    if lower.contains("shift+tab to cycle") || lower.contains("? for shortcuts") {
        return true;
    }
    if empty_prompt(row) {
        return true;
    }
    row.chars().count() <= 40
        && (lower.contains("ctrl+p commands")
            || lower.contains("esc interrupt")
            || lower.contains("esc to interrupt"))
}

/// A row that is nothing but the glyph a CLI draws its prompt with: the
/// composer with nothing typed in it. What is typed there is content and
/// stays; the empty box is chrome.
fn empty_prompt(row: &str) -> bool {
    let mut glyphs = row.trim().chars();
    matches!(glyphs.next(), Some('❯' | '›' | '»' | '>')) && glyphs.next().is_none()
}

/// A usage figure: a number, an optional unit letter, and a percentage in
/// parentheses right after it — the "146.5K (56%)" of a context bar. The rest
/// of the row may continue, so the percentage is matched in place, not at the
/// end. A bare percent in prose does not match, because it has no figure
/// before it.
fn usage_figure(row: &str) -> bool {
    let bytes = row.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if !bytes[i].is_ascii_digit() {
            i += 1;
            continue;
        }
        let mut j = i;
        while j < bytes.len() && (bytes[j].is_ascii_digit() || bytes[j] == b'.') {
            j += 1;
        }
        if j < bytes.len() && bytes[j].is_ascii_alphabetic() {
            j += 1;
        }
        if j + 1 < bytes.len() && bytes[j] == b' ' && bytes[j + 1] == b'(' {
            let mut p = j + 2;
            while p < bytes.len() && bytes[p].is_ascii_digit() {
                p += 1;
            }
            if p + 2 <= bytes.len() && bytes[p] == b'%' && bytes[p + 1] == b')' {
                return true;
            }
        }
        i = j.max(i + 1);
    }
    false
}

/// A row that ends on a bare version stamp — "1.18.23", "v2.0.1" — the way a
/// footer names the build that drew it.
fn ends_with_version(row: &str) -> bool {
    let Some(last) = row.split_whitespace().next_back() else {
        return false;
    };
    let last = last.strip_prefix('v').unwrap_or(last);
    let parts: Vec<&str> = last.split('.').collect();
    (2..=4).contains(&parts.len())
        && parts
            .iter()
            .all(|part| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit()))
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
    state: Mutex<State>,
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
            state: Mutex::new(state),
            state_path,
        })
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// The name a machine argument goes by here. This machine is `local` to
    /// the controller, but every daemon already calls its own machine that, so
    /// the fleet was told this one's own name instead (see `relay::run_pump`),
    /// and that is the name machine lists here hand out. A call naming it
    /// means here, not a machine that is missing. An ssh alias of the same
    /// spelling wins: it is the more deliberate answer, and it points at this
    /// host anyway.
    fn spelled_here<'a>(&self, machine: &'a str) -> &'a str {
        if !self.state().enabled_hosts.contains(machine)
            && machine.eq_ignore_ascii_case(crate::model::own_machine_name())
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
        if !self.state().enabled_hosts.contains(machine) {
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
            .state()
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
            // Its own name, not `local`: an id is what gets copied into the
            // next call and quoted back to a person, and `local` names a
            // different machine on every node that says it. `local` still
            // addresses this one - `spelled_here` takes either.
            "id": crate::model::own_machine_name(),
            "label": crate::model::own_machine_name(),
            "enabled": self.state().enabled_hosts.contains(crate::model::LOCAL_TARGET_ID),
            "connected": self.runtime.bridge_pool().is_connected(crate::model::LOCAL_TARGET_ID),
        })];
        let aliases = ssh_config::load_hosts(&self.config.ssh_config_path()).unwrap_or_default();
        for alias in aliases {
            machines.push(json!({
                "id": alias,
                "label": alias,
                "enabled": self.state().enabled_hosts.contains(&alias),
                "connected": self.runtime.bridge_pool().is_connected(&alias),
            }));
        }
        Ok(pretty(&Value::Array(machines)))
    }

    /// Add a machine to the reachable set or take it out of it. The state file
    /// is read again first: the dashboard owns the same file, and an MCP
    /// process that started an hour ago must not write back a stale view.
    fn set_machine_enabled(&self, arguments: &Value) -> Result<String> {
        // Spelled here first: machine lists hand this one out under its own
        // name, and the name they printed has to be the name that works.
        let machine = self
            .spelled_here(required_str(arguments, "machine")?)
            .to_string();
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
        *self.state() = state;
        Ok(pretty(&json!({
            "machine": crate::model::machine_read_as(&machine),
            "enabled": enabled,
            "changed": changed,
            "enabled_machines": self
                .state()
                .enabled_hosts
                .iter()
                .map(|host| crate::model::machine_read_as(host))
                .collect::<Vec<_>>(),
        })))
    }

    /// Read the SSH aliases this machine knows, or write one into the file
    /// muxloom owns. Hosts the user maintains are read but never rewritten.
    fn ssh_host(&self, arguments: &Value) -> Result<String> {
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
                            "enabled": self.state().enabled_hosts.contains(&alias),
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
                    "enabled": self.state().enabled_hosts.contains(&alias),
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
                *self.state() = state;
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
            // Nobody asked for the archive: don't make the machine send it.
            // The filter below used to be where every record of every
            // conversation the machine has ever held was thrown away, after
            // being serialized, carried here - over ssh, for another machine -
            // and parsed. What is left in the live list and still archived is
            // a session put down since this daemon started, which the filter
            // still has to catch.
            let pool = self.runtime.bridge_pool();
            let listed = match include_archived {
                true => pool.list_sessions(&target),
                false => pool.list_live_sessions(&target),
            };
            match listed {
                Ok(sessions) => rendered.extend(
                    sessions
                        .iter()
                        .filter(|session| include_archived || !session.archived)
                        .map(|session| session_json(&target.id, session)),
                ),
                Err(error) => rendered.push(json!({
                    "machine": crate::model::machine_read_as(&target.id),
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
        let raw = optional_bool(arguments, "raw");
        let page = self
            .runtime
            .screen_page(&target, session_id, offset, lines)?;
        Ok(screen_page(&page, raw))
    }

    fn wait_for(&self, arguments: &Value) -> Result<String> {
        let target = self.target(arguments)?;
        let session_id = required_str(arguments, "session_id")?.to_string();
        let pool = self.runtime.bridge_pool();
        wait_loop(
            arguments,
            &target.id,
            || {
                waited_session(
                    &session_id,
                    || pool.live_session(&target, &session_id),
                    || pool.list_sessions(&target),
                )
            },
            || {
                let page = self
                    .runtime
                    .screen_page(&target, &session_id, 0, WAIT_SCREEN_LINES)?;
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
        let pool = self.runtime.bridge_pool();
        let now = session_name_now(|id| pool.live_session(&Target::local(), id));
        let author = message_author(now, || pool.talk_status(&Target::local(), None));
        let message = pool.talk_post(&target, talk_draft(arguments, author)?)?;
        Ok(pretty(&talk_json(&message)))
    }

    fn talk_read(&self, arguments: &Value) -> Result<String> {
        let target = self.target(arguments)?;
        let pool = self.runtime.bridge_pool();
        talk_wait(
            arguments,
            talk_filter(arguments)?,
            |filter| pool.talk_read(&target, filter.clone()),
            |session_id| pool.known_session(&target, session_id).ok().flatten(),
        )
    }

    /// The channel bindings as the dashboard last wrote them. Read on every
    /// call rather than held: this surface outlives any one binding, and a
    /// human who has just pressed `c` should not have to restart anything.
    fn channels(&self) -> Result<crate::channel::ChannelSet> {
        crate::channel::ChannelSet::load(&crate::channel::path_in(
            self.state_path.parent().unwrap_or_else(|| Path::new(".")),
        ))
    }

    fn send_channel_message(&self, arguments: &Value) -> Result<String> {
        let channels = self.channels()?;
        let binding = channels.pick(optional_str(arguments, "channel"))?;
        let environment = self.config.environment_for(crate::model::LOCAL_TARGET_ID)?;
        // Left with the daemon on this machine rather than kept here: this
        // process ends when the tool call does, and the dashboard that reads
        // the chat is somewhere else entirely.
        let now = session_name_now(|id| {
            self.runtime
                .bridge_pool()
                .live_session(&Target::local(), id)
        });
        send_channel(binding, arguments, &environment, now, |receipt| {
            let _ = self
                .runtime
                .bridge_pool()
                .channel_sent(&Target::local(), receipt);
        })
    }

    fn message_agent(&self, arguments: &Value) -> Result<String> {
        let target = self.target(arguments)?;
        let pool = self.runtime.bridge_pool();
        let now = session_name_now(|id| pool.live_session(&Target::local(), id));
        let author = message_author(now, || pool.talk_status(&Target::local(), None));
        let (draft, deliver, reply_expected) = direct_draft(arguments, author)?;
        let (message, delivery, reason) =
            pool.talk_deliver(&target, draft, deliver, reply_expected)?;
        Ok(delivery_json(&message, &delivery, reason))
    }

    fn launch_session(&self, arguments: &Value) -> Result<String> {
        let target = self.target(arguments)?;
        // A resume_id naming a muxloom session is an id-stable fleet resume:
        // the archived session comes back as itself - same number, label,
        // parent, history - and everything that still hangs off it by parent
        // link comes back with it. A resume_id that names an agent-native
        // conversation instead stays the ordinary relaunch below.
        //
        // Which of the two it is, is read off the id before the machine is
        // asked for anything. Listing the sessions draws the screen of every
        // one of them, and a resume_id naming an agent-native conversation
        // walks straight past that list without looking at it -- so an
        // ordinary relaunch was paying for a fleet listing it never read, and
        // failing outright if the listing failed.
        if let Some(resume_id) = optional_str(arguments, "resume_id")
            && crate::runtime::is_daemon_session_id(resume_id)
        {
            let sessions = self.runtime.bridge_pool().list_sessions(&target)?;
            if let Some(master) = fleet_resume_target(&sessions, resume_id)? {
                return self.resume_fleet(&target, master, arguments);
            }
        }
        let kind = agent_kind(arguments)?;
        // A relayed launch was already weighed against its caller's powers on
        // the machine that could read them, and arrives holding the grant that
        // came out of it. A launch made here is a person's, and a person's
        // agent answers to the person.
        let powers = match relayed_powers(arguments) {
            Some(granted) => granted,
            None => {
                let own = own_powers();
                check_may_launch(&own, kind)?;
                granted_powers(arguments, &own)?
            }
        };
        let request = LaunchRequest {
            target: target.clone(),
            kind,
            path: required_str(arguments, "path")?.into(),
            label: optional_str(arguments, "label").unwrap_or_default().into(),
            temporary: false,
            resume_id: optional_str(arguments, "resume_id").map(Into::into),
            revive: None,
            initial_prompt: optional_str(arguments, "initial_prompt").map(Into::into),
            parent: relayed_caller(arguments),
            powers: Some(powers),
        };
        let command = self.config.command_for(&target.id, kind).clone();
        let environment = self.config.environment_for(&target.id)?;
        let session_id = self.runtime.launch(&request, &command, &environment)?;
        Ok(pretty(&json!({
            "session_id": session_id,
            "machine": crate::model::machine_read_as(&target.id),
            "kind": kind.as_str(),
            "path": request.path,
            "parent": request.parent,
        })))
    }

    /// Resume one archived session and its whole subtree on the numbers its
    /// records already hold, through the bridge directly: `Runtime::launch`
    /// mints a fresh id, which is exactly the mistake that split fleets
    /// children off from their masters. Children first - each on its own
    /// record, with its own native conversation when the runtime can reopen
    /// one and a synthetic first prompt when it cannot - and the master last,
    /// carrying the caption that says what came back and what did not.
    fn resume_fleet(
        &self,
        target: &Target,
        master: &DaemonSession,
        arguments: &Value,
    ) -> Result<String> {
        let environment = self.config.environment_for(&target.id)?;
        // What the master needs to come back is worked out before a single
        // child is touched. A fleet resume that cannot end with the master
        // running must not begin by relaunching everyone under it: they would
        // come back pointed at a number that answers nothing, and the only
        // account of it would be an error about the master.
        let master_kind = master
            .kind
            .parse::<AgentKind>()
            .map_err(|error: String| anyhow::anyhow!(error))?;
        let master_command = self.config.command_for(&target.id, master_kind).clone();
        if master_command.command.trim().is_empty() && master_kind != AgentKind::Terminal {
            bail!("command for {master_kind} is empty; the master cannot come back either");
        }
        let sessions = self.runtime.bridge_pool().list_sessions(target)?;
        let (plan, truncated) = fleet_resume_plan(&sessions, &master.id);
        let pool = self.runtime.bridge_pool();
        let mut outcomes: Vec<FleetOutcome> = Vec::new();
        for member in &plan {
            let outcome = |status: &'static str,
                           resumed_with: Option<String>,
                           detail: Option<String>| FleetOutcome {
                record: member.record.clone(),
                status,
                resumed_with,
                detail,
            };
            match member.action {
                FleetMemberAction::Running => {
                    outcomes.push(outcome("running", None, None));
                    continue;
                }
                FleetMemberAction::Ephemeral => {
                    outcomes.push(outcome("temporary", None, None));
                    continue;
                }
                FleetMemberAction::Relaunch => {}
            }
            let Ok(kind) = member.record.kind.parse::<AgentKind>() else {
                outcomes.push(outcome(
                    "unresumed",
                    None,
                    Some(format!("unknown kind {}", member.record.kind)),
                ));
                continue;
            };
            let command = self.config.command_for(&target.id, kind).clone();
            if command.command.trim().is_empty() && kind != AgentKind::Terminal {
                outcomes.push(outcome(
                    "unresumed",
                    None,
                    Some(format!("no {} command configured", kind.as_str())),
                ));
                continue;
            }
            let resume_with = member.native.clone();
            let args = crate::runtime::launch_arguments(
                &command,
                kind,
                false,
                resume_with.as_deref(),
                None,
            );
            let synthetic = resume_with
                .is_none()
                .then(|| synthetic_child_prompt(&member.record));
            let launched = pool.launch(
                target,
                member.record.id.clone(),
                member.record.kind.clone(),
                member.record.path.clone(),
                String::new(),
                false,
                command.command.clone(),
                args,
                environment.clone(),
                member.record.created_at,
                Some(master.id.clone()),
                // A resume restores the powers off the record it revives, so a
                // session cannot come back holding more than it died with.
                None,
                synthetic,
                // Nothing to tell it: a member coming back on its own thread
                // already carries the words that thread opened with, and one
                // coming back without a thread is typed the caption above,
                // which the recorder hears for itself.
                None,
            );
            match launched {
                Ok(_) => outcomes.push(outcome(
                    if resume_with.is_some() {
                        "restored"
                    } else {
                        "fresh"
                    },
                    resume_with,
                    None,
                )),
                Err(error) => outcomes.push(outcome(
                    "unresumed",
                    resume_with,
                    Some(format!("{error:#}")),
                )),
            }
        }
        let resume_with = native_resume_id(master).map(str::to_string);
        let args = crate::runtime::launch_arguments(
            &master_command,
            master_kind,
            false,
            resume_with.as_deref(),
            None,
        );
        let caption = fleet_resume_caption(&master.id, &master.label, &outcomes, truncated);
        let label = optional_str(arguments, "label")
            .unwrap_or_default()
            .replace(['\t', '\n', '\r'], " ");
        // The daemon revives the record in place: the same number answers,
        // and the record's own label and parent survive an empty request.
        let session = pool
            .launch(
                target,
                master.id.clone(),
                master.kind.clone(),
                master.path.clone(),
                label,
                false,
                master_command.command.clone(),
                args,
                environment,
                master.created_at,
                relayed_caller(arguments),
                None,
                Some(caption),
                // A resume, so the same as its members: the caption is typed
                // in and the record already holds the opening.
                None,
            )
            .map_err(|error| match fleet_already_back(&master.id, &outcomes) {
                Some(note) => error.context(note),
                None => error,
            })?;
        Ok(pretty(&json!({
            "session_id": session.id,
            "machine": crate::model::machine_read_as(&target.id),
            "kind": session.kind,
            "path": session.path,
            "label": session.label,
            "parent": session.parent,
            "resumed": true,
            "fleet": fleet_outcome_json(&outcomes),
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
        let deep = optional_bool(arguments, "deep");
        let mut results = Vec::new();
        for target in self.targets(arguments)? {
            // Asked about the whole machine, the machine is asked once. Walking
            // it from here meant fetching the session list — which draws every
            // live screen to answer — and then one round trip per session, for
            // hundreds of captures that mostly do not hold the word.
            let (hits, skipped) = match optional_str(arguments, "session_id") {
                Some(session_id) => {
                    match self.runtime.search_history(
                        &target,
                        session_id,
                        query,
                        SEARCH_MAX_MATCHES,
                    ) {
                        // One named session is read whole however old it is:
                        // the pool exists to keep a search off captures nobody
                        // asked about, and this one was asked about.
                        Ok(matches) => (
                            vec![HistorySearchHit {
                                session_id: session_id.into(),
                                label: String::new(),
                                matches,
                            }],
                            0,
                        ),
                        Err(_) => continue,
                    }
                }
                None => {
                    match self
                        .runtime
                        .search_history_all(&target, query, SEARCH_MAX_MATCHES, deep)
                    {
                        Ok(sweep) => (sweep.hits, sweep.skipped),
                        Err(error) => {
                            results.push(json!({
                                "machine": crate::model::machine_read_as(&target.id),
                                "error": format!("{error:#}"),
                            }));
                            continue;
                        }
                    }
                }
            };
            // A search that stopped short says so. Coming back empty from a
            // pool that never opened the file reads exactly like a machine
            // where the word was never said, and that is the one wrong answer
            // this is allowed to give.
            if skipped > 0 {
                results.push(json!({
                    "machine": crate::model::machine_read_as(&target.id),
                    "unsearched_sessions": skipped,
                    "note": format!(
                        "Searched the recently written captures on {}; {skipped} older ones were \
                         not read. Only if this answer is genuinely not enough, ask again with \
                         deep=true - that reads every capture on the machine, which can run to \
                         gigabytes and take seconds.",
                        target.id,
                    ),
                }));
            }
            for hit in hits {
                if hit.matches.is_empty() {
                    continue;
                }
                results.push(json!({
                    "machine": crate::model::machine_read_as(&target.id),
                    "session_id": hit.session_id,
                    "label": hit.label,
                    "matches": hit.matches
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
                .state()
                .enabled_hosts
                .iter()
                .map(|host| index.machine_key_for_alias(host))
                .collect());
        }
        asked
            .iter()
            .map(|machine| {
                let machine = self.spelled_here(machine);
                if !self.state().enabled_hosts.contains(machine) {
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
        use crate::backup::{BackupStore, SearchFilter, search_index};

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
        // The index was loaded to work out which machines may be searched;
        // the search reads the same one rather than parsing the file again.
        let hits = search_index(&store, &index, query, limit, &filter)?;
        let rendered: Vec<Value> = hits
            .iter()
            .map(|hit| {
                json!({
                    "machine": crate::model::machine_read_as(&hit.target_id),
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
                     happening right now may be missing its last turns; read_conversation reads \
                     a claude, codex or pi conversation on this machine off its own transcript, \
                     which has them.",
        })))
    }

    #[cfg(not(feature = "controller"))]
    fn search_conversations(&self, _arguments: &Value) -> Result<String> {
        bail!("this muxloom build keeps no conversation backup to search")
    }

    #[cfg(feature = "controller")]
    fn read_conversation(&self, arguments: &Value) -> Result<String> {
        use crate::backup::{BackupStore, read_conversation};

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
        let read = read_conversation(&store, record, from, limit)?;
        let (window, total, source) = (read.messages, read.total, read.source);

        let (messages, next, clipped_any) = conversation_page(&window, max_chars);
        let after = window
            .last()
            .map(|(position, _)| position + 1)
            .unwrap_or(from);
        let next_cursor = next
            .filter(|resume| *resume < total)
            .or((after < total).then_some(after));
        Ok(pretty(&json!({
            "machine": crate::model::machine_read_as(&record.target_id),
            "session_id": record.session_id,
            "kind": record.kind,
            "path": record.cwd,
            "title": record.title,
            "total_messages": total,
            "from_index": from,
            "source": source.as_str(),
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

    fn call(&self, name: &str, arguments: &Value) -> Result<String> {
        enforce_policy(&self.config.mcp, name)?;
        // A moderator is served by this surface rather than the daemon's, and
        // a moderator can have been started by another agent like anyone else:
        // the three other dials are already weighed here, and reach was the one
        // that was not. The list to walk it against is the target machine's,
        // because this surface can aim at any of them.
        if let Some(session_id) = written_to(name, arguments) {
            let own = own_powers();
            if own.reach != Reach::Fleet && !reaches_without_records(&own, session_id) {
                // Parent links alone, and asked for as such. This runs before
                // every message and every keystroke one agent sends another,
                // and asking for the sessions instead drew every screen on the
                // machine and carried its whole archive back to read two fields
                // off each record — which is what made talking to a sibling the
                // slowest thing an agent could do.
                let parents = self
                    .runtime
                    .bridge_pool()
                    .lineage(&self.target(arguments)?)?;
                check_may_message(&own, session_id, &parents)?;
            }
        }
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
            "send_channel_message" => self.send_channel_message(arguments),
            "send_input" => {
                let target = self.target(arguments)?;
                let session_id = required_str(arguments, "session_id")?;
                // Only a running session can be typed into, so the archive
                // has nothing to say about which one this is.
                let kind = session_kind(
                    self.runtime
                        .bridge_pool()
                        .live_session(&target, session_id)
                        .ok()
                        .flatten(),
                );
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
        DEFAULT_SCREEN_LINES, Flavor, FleetMemberAction, FleetOutcome, SEARCH_MAX_MATCHES,
        WAIT_SCREEN_LINES, agent_kind, allowed_specs, build_input, check_may_launch,
        check_may_message, check_may_reach_person, delivery_json, direct_draft, enforce_policy,
        fleet_already_back, fleet_outcome_json, fleet_resume_caption, fleet_resume_plan,
        fleet_resume_target, granted_powers, instructions, launch_path_within, launching_session,
        lineage, lineage_of_answer, message_author, native_resume_id, optional_bool, optional_str,
        optional_usize, own_powers, plain_screen, pretty, preview_text, reaches_without_records,
        required_str, screen_page, send_channel, session_env, session_json, session_kind,
        session_name_now, shell_report, stamp_powers, synthetic_child_prompt, talk_draft,
        talk_filter, talk_json, talk_wait, trigger_json, trigger_spec, wait_loop, waited_session,
        written_to,
    };
    use crate::{
        channel::ChannelSet,
        config::{Config, default_config_path},
        daemon::{DaemonPaths, connect_or_start},
        daemon_protocol::{
            DaemonHistoryMatch, DaemonRequest, DaemonResponse, DaemonSession, Frame, FrameKind,
            Trigger, stream,
        },
        model::{AgentKind, LOCAL_TARGET_ID, Reach},
        runtime::{ScreenRead, launch_arguments, launch_seed, new_daemon_session_id},
    };

    /// How long one daemon request may run. Matches the bridge's own request
    /// timeout: a shell script is the slowest thing a request can carry.
    const REQUEST_TIMEOUT: Duration = Duration::from_secs(180);
    /// The most preview bytes a tool answer carries.
    const PREVIEW_LIMIT: usize = 256 * 1024;
    /// How long a call the controller runs for us may take. The call itself
    /// takes as long as it takes; a minute covers a search across a slow link,
    /// and failing after it is better than being dropped by the client
    /// mid-call.
    const RELAY_WAIT: Duration = Duration::from_secs(60);
    /// How soon after submitting we look for an answer, and how much longer we
    /// wait each time none has come. A controller on the same machine can be
    /// done in a few milliseconds, so the first look is nearly free and the
    /// gaps grow only for a call that is genuinely still running — a fixed
    /// quarter-second would have charged every quick answer the full quarter.
    const RELAY_POLL_FIRST: Duration = Duration::from_millis(15);
    const RELAY_POLL_MAX: Duration = Duration::from_millis(250);

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

    /// One session a search had something to say about: its id, the label to
    /// name it by, and the lines the word was found on.
    type SearchedSession = (String, String, Vec<DaemonHistoryMatch>);

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
            self.session_list(false, None)
        }

        /// Only what the daemon is running. A lineage walk wants the whole
        /// list — a parent can be archived — but everything that asks only
        /// what is going on now should ask for that.
        fn live_sessions(&self) -> Result<Vec<DaemonSession>> {
            self.session_list(true, None)
        }

        fn session_list(
            &self,
            live_only: bool,
            only: Option<String>,
        ) -> Result<Vec<DaemonSession>> {
            match self
                .transact(&DaemonRequest::ListSessions { live_only, only })?
                .0
            {
                DaemonResponse::Sessions { sessions } => Ok(sessions),
                response => bail!("unexpected session-list response: {response:?}"),
            }
        }

        /// One running session, for a round that is about one session.
        ///
        /// A daemon too old to read the id answers with all of them, so the
        /// find is here rather than assumed away.
        fn live_session(&self, session_id: &str) -> Result<Option<DaemonSession>> {
            Ok(self
                .session_list(true, Some(session_id.to_string()))?
                .into_iter()
                .find(|session| session.id == session_id))
        }

        /// One session by name, running or filed, for the questions an
        /// archived session still answers.
        fn known_session(&self, session_id: &str) -> Result<Option<DaemonSession>> {
            Ok(self
                .session_list(false, Some(session_id.to_string()))?
                .into_iter()
                .find(|session| session.id == session_id))
        }

        /// Who begat whom, without the sessions themselves.
        ///
        /// This surface negotiates no capabilities, so the fallback answers the
        /// version question instead of a handshake: a daemon too old for this
        /// says so, and gets asked the old expensive way.
        fn parentage(&self) -> Result<Vec<(String, Option<String>)>> {
            match self.transact(&DaemonRequest::Lineage) {
                Ok((DaemonResponse::Parents { parents }, _)) => Ok(parents),
                _ => Ok(lineage(&self.sessions()?)),
            }
        }

        /// Weigh a write aimed at one of this machine's sessions against how
        /// far this session may speak.
        ///
        /// Reading the list is a round trip to the daemon, so the full reach —
        /// which asks nothing of a lineage — never pays for it. What the rest
        /// pay is two fields per session: this runs before every message one
        /// agent sends another, and asking for the sessions themselves drew
        /// every screen on the machine and carried its whole archive back to
        /// throw all but the parent links away.
        fn check_reach(&self, session_id: &str) -> Result<()> {
            let own = own_powers();
            if own.reach == Reach::Fleet || reaches_without_records(&own, session_id) {
                return Ok(());
            }
            check_may_message(&own, session_id, &self.parentage()?)
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
        fn screen_rows(&self, session_id: &str, offset: usize, lines: usize) -> Result<ScreenRead> {
            let (response, data) = self.transact(&DaemonRequest::ReadHistory {
                session_id: session_id.into(),
                offset_from_bottom: offset,
                lines,
                rendered: true,
                from_drawn: true,
            })?;
            match response {
                DaemonResponse::HistoryComplete {
                    total_lines,
                    offset_from_bottom,
                    reached_start,
                    alternate_screen,
                    ..
                } => {
                    let text = String::from_utf8_lossy(
                        data.get(&stream::HISTORY).map_or(&[][..], Vec::as_slice),
                    );
                    Ok(ScreenRead {
                        text: text.trim_end().to_string(),
                        offset_from_bottom,
                        total_rows: total_lines,
                        reached_start,
                        alternate_screen,
                    })
                }
                response => bail!("unexpected history response: {response:?}"),
            }
        }

        fn read_screen(&self, arguments: &Value) -> Result<String> {
            let session_id = required_str(arguments, "session_id")?;
            let lines = optional_usize(arguments, "lines", DEFAULT_SCREEN_LINES);
            let offset = optional_usize(arguments, "offset_from_bottom", 0);
            let raw = optional_bool(arguments, "raw");
            let page = self.screen_rows(session_id, offset, lines)?;
            Ok(screen_page(&page, raw))
        }

        fn wait_for(&self, arguments: &Value) -> Result<String> {
            let session_id = required_str(arguments, "session_id")?.to_string();
            wait_loop(
                arguments,
                LOCAL_TARGET_ID,
                // The same round the controller's wait makes, and this is the
                // copy every agent living on the machine calls.
                || {
                    waited_session(
                        &session_id,
                        || self.live_session(&session_id),
                        || self.sessions(),
                    )
                },
                || {
                    let text = self.screen_rows(&session_id, 0, WAIT_SCREEN_LINES)?.text;
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
            let now = session_name_now(|id| self.live_session(id));
            let author = message_author(now, || {
                match self.transact(&DaemonRequest::TalkStatus { label: None })?.0 {
                    DaemonResponse::TalkBoard { state } => Ok(state),
                    response => bail!("unexpected talk response: {response:?}"),
                }
            });
            let draft = talk_draft(arguments, author)?;
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
                |session_id| self.known_session(session_id).ok().flatten(),
            )
        }

        /// Say something to the human, from this machine.
        ///
        /// The credentials live here because the message goes out from here:
        /// this machine may be the only one awake, and borrowing a controller's
        /// network to speak would make "tell me when it is done" depend on
        /// somebody's laptop being open.
        ///
        /// Which leaves the window before the set arrives. Bindings are carried
        /// on the same round as the talk board, so a machine that was enabled a
        /// moment ago, or rebuilt, or whose daemon has just started, has none
        /// for a second or two. Nothing has touched the network at that point,
        /// so asking the controller to send instead costs nothing and cannot
        /// deliver the message twice. Past that first step muxloom never tries
        /// again from somewhere else: a post that failed may still have
        /// arrived, and a person reading the same thing twice cannot tell which
        /// of the two was the real one.
        fn send_channel_message(&self, arguments: &Value) -> Result<String> {
            let channels = ChannelSet::read(&self.paths.channels);
            let environment = self.config.environment_for(LOCAL_TARGET_ID)?;
            match channels.pick(optional_str(arguments, "channel")) {
                Ok(binding) => {
                    let now = session_name_now(|id| self.live_session(id));
                    send_channel(binding, arguments, &environment, now, |receipt| {
                        let _ = self.transact(&DaemonRequest::ChannelSent { receipt });
                    })
                }
                Err(unbound) => self
                    .relay("send_channel_message", arguments)
                    // The controller could not help either. What a human can
                    // act on is that nothing here is bound, not that an errand
                    // went unanswered, so that is what comes back.
                    .map_err(|_| unbound),
            }
        }

        /// Whether these arguments name a machine other than this one, in
        /// which case the call is the controller's to make. Neither this
        /// machine's own name - which is what its answers hand out - nor
        /// `local`, which every daemon calls itself, ever travels. Past that
        /// the board knows both names this machine goes by: the key it mints
        /// messages under and the label the controller calls it. Not
        /// knowing them is not fatal — the errand comes back to this daemon
        /// and is answered here, one hop later than it needed to be.
        fn elsewhere(&self, arguments: &Value) -> Option<String> {
            let machine = optional_str(arguments, "machine")?;
            if machine == LOCAL_TARGET_ID || machine == crate::model::own_machine_name() {
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
                    session: launching_session().unwrap_or_default(),
                })?
                .0
            {
                DaemonResponse::RelayTicket { id } => id,
                response => bail!("unexpected relay response: {response:?}"),
            };
            let deadline = Instant::now() + RELAY_WAIT;
            let mut gap = RELAY_POLL_FIRST;
            loop {
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
                std::thread::sleep(gap);
                gap = (gap * 2).min(RELAY_POLL_MAX);
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
                // As above: the name is the id here too, and `elsewhere`
                // knows it as this machine.
                "id": crate::model::own_machine_name(),
                "label": own.map_or_else(
                    || crate::model::own_machine_name().to_string(),
                    |peer| peer.label.clone(),
                ),
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
            // Reach is weighed in `call`, where every door into another
            // session's prompt box is weighed together.
            //
            // The board here is the one the message will be filed on, so an
            // author with no machine on it would be right anyway; asking keeps
            // the record the same shape as one that crossed a machine.
            let now = session_name_now(|id| self.live_session(id));
            let author = message_author(now, || {
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
            // A resume_id naming one of this machine's own archived sessions
            // is an id-stable fleet resume, answered before the ordinary
            // launch's rules - the folder rule below is re-checked inside it
            // for every session it would relaunch. A resume_id naming an
            // agent-native conversation stays the ordinary relaunch, and is
            // told apart before the sessions are asked for: that listing draws
            // every session's screen, and the ordinary relaunch never reads it.
            if let Some(resume_id) = optional_str(arguments, "resume_id")
                && crate::runtime::is_daemon_session_id(resume_id)
            {
                let sessions = self.sessions()?;
                if let Some(master) = fleet_resume_target(&sessions, resume_id)? {
                    return self.resume_fleet(master, arguments);
                }
            }
            let kind = agent_kind(arguments)?;
            let powers = own_powers();
            check_may_launch(&powers, kind)?;
            let powers = granted_powers(arguments, &powers)?;
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
            let seed = launch_seed(
                kind,
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
                    powers: Some(powers),
                    initial_prompt: seed,
                    // What this session is being started to do, handed over
                    // so it can show which conversation is its own later.
                    // The prompt travels in the command line for every
                    // runtime but OpenCode, so unless it is said here the
                    // daemon never hears it. Withheld on a resume, whose
                    // prompt reopens a thread rather than beginning one.
                    first_prompt: optional_str(arguments, "initial_prompt")
                        .filter(|_| optional_str(arguments, "resume_id").is_none())
                        .map(str::to_string),
                })?
                .0;
            match response {
                DaemonResponse::Launched { session } => Ok(pretty(&json!({
                    "session_id": session.id,
                    "machine": crate::model::own_machine_name(),
                    "kind": session.kind,
                    "path": session.path,
                    "parent": session.parent,
                }))),
                response => bail!("unexpected launch response: {response:?}"),
            }
        }

        /// Resume one archived session and its subtree on the numbers their
        /// records already hold, scoped to the caller's own folder exactly as
        /// an ordinary launch is: a child recorded somewhere else belongs to
        /// the agent that lives there, not to whoever resumes the master, and
        /// the caption says so rather than quietly launching across folders.
        /// Children first, master last with the caption as its first prompt.
        fn resume_fleet(&self, master: &DaemonSession, arguments: &Value) -> Result<String> {
            let own = self.own_folder.as_deref().context(
                "resume_fleet brings back sessions where you are, and muxloom cannot tell which \
                 folder that is",
            )?;
            if !crate::moderator::within(own, &master.path) {
                bail!(
                    "launch_session resumes sessions in your own folder, {own}, and {} is \
                     outside it. Ask the agent that lives there, or a muxloom moderator.",
                    master.path
                );
            }
            let environment = self.config.environment_for(LOCAL_TARGET_ID)?;
            // Asked before a single child is touched: the master's own way
            // back does not depend on any of them, and a resume that cannot
            // end with the master running must not begin by putting a fleet
            // under a number that will answer nothing.
            let master_kind = master
                .kind
                .parse::<AgentKind>()
                .map_err(|error: String| anyhow::anyhow!(error))?;
            let master_command = self
                .config
                .command_for(LOCAL_TARGET_ID, master_kind)
                .clone();
            if master_command.command.trim().is_empty() && master_kind != AgentKind::Terminal {
                bail!("command for {master_kind} is empty; the master cannot come back either");
            }
            let sessions = self.sessions()?;
            let (plan, truncated) = fleet_resume_plan(&sessions, &master.id);
            let mut outcomes: Vec<FleetOutcome> = Vec::new();
            for member in &plan {
                let outcome = |status: &'static str,
                               resumed_with: Option<String>,
                               detail: Option<String>| FleetOutcome {
                    record: member.record.clone(),
                    status,
                    resumed_with,
                    detail,
                };
                match member.action {
                    FleetMemberAction::Running => {
                        outcomes.push(outcome("running", None, None));
                        continue;
                    }
                    FleetMemberAction::Ephemeral => {
                        outcomes.push(outcome("temporary", None, None));
                        continue;
                    }
                    FleetMemberAction::Relaunch => {}
                }
                if !crate::moderator::within(own, &member.record.path) {
                    outcomes.push(outcome(
                        "out_of_scope",
                        None,
                        Some(format!("{} is outside your folder", member.record.path)),
                    ));
                    continue;
                }
                let Ok(kind) = member.record.kind.parse::<AgentKind>() else {
                    outcomes.push(outcome(
                        "unresumed",
                        None,
                        Some(format!("unknown kind {}", member.record.kind)),
                    ));
                    continue;
                };
                let command = self.config.command_for(LOCAL_TARGET_ID, kind).clone();
                if command.command.trim().is_empty() && kind != AgentKind::Terminal {
                    outcomes.push(outcome(
                        "unresumed",
                        None,
                        Some(format!("no {} command configured", kind.as_str())),
                    ));
                    continue;
                }
                let resume_with = member.native.clone();
                let args = launch_arguments(&command, kind, false, resume_with.as_deref(), None);
                let synthetic = resume_with
                    .is_none()
                    .then(|| synthetic_child_prompt(&member.record));
                let request = DaemonRequest::Launch {
                    session_id: member.record.id.clone(),
                    kind: member.record.kind.clone(),
                    path: member.record.path.clone(),
                    label: String::new(),
                    temporary: false,
                    executable: command.command.clone(),
                    args,
                    environment: environment.clone(),
                    created_at: member.record.created_at,
                    columns: 120,
                    rows: 40,
                    parent: Some(master.id.clone()),
                    // A resume restores the powers off the record it revives,
                    // so a session cannot come back holding more than it died
                    // with.
                    powers: None,
                    initial_prompt: synthetic,
                    // Nothing to tell it: a member coming back on its own
                    // thread already carries the words that thread opened
                    // with, and one coming back without a thread is typed
                    // the caption above, which the recorder hears for
                    // itself.
                    first_prompt: None,
                };
                match self.transact(&request) {
                    Ok((DaemonResponse::Launched { .. }, _)) => outcomes.push(outcome(
                        if resume_with.is_some() {
                            "restored"
                        } else {
                            "fresh"
                        },
                        resume_with,
                        None,
                    )),
                    Ok((response, _)) => outcomes.push(outcome(
                        "unresumed",
                        resume_with,
                        Some(format!("unexpected launch response: {response:?}")),
                    )),
                    Err(error) => outcomes.push(outcome(
                        "unresumed",
                        resume_with,
                        Some(format!("{error:#}")),
                    )),
                }
            }
            let resume_with = native_resume_id(master).map(str::to_string);
            let args = launch_arguments(
                &master_command,
                master_kind,
                false,
                resume_with.as_deref(),
                None,
            );
            let caption = fleet_resume_caption(&master.id, &master.label, &outcomes, truncated);
            let response = self
                .transact(&DaemonRequest::Launch {
                    session_id: master.id.clone(),
                    kind: master.kind.clone(),
                    path: master.path.clone(),
                    label: optional_str(arguments, "label")
                        .unwrap_or_default()
                        .replace(['\t', '\n', '\r'], " "),
                    temporary: false,
                    executable: master_command.command.clone(),
                    args,
                    environment,
                    created_at: master.created_at,
                    columns: 120,
                    rows: 40,
                    parent: launching_session(),
                    powers: None,
                    initial_prompt: Some(caption),
                    // A resume, so the same as its members: the caption is
                    // typed in and the record already holds the opening.
                    first_prompt: None,
                })
                .map_err(|error| match fleet_already_back(&master.id, &outcomes) {
                    Some(note) => error.context(note),
                    None => error,
                })?
                .0;
            match response {
                DaemonResponse::Launched { session } => Ok(pretty(&json!({
                    "session_id": session.id,
                    "machine": crate::model::own_machine_name(),
                    "kind": session.kind,
                    "path": session.path,
                    "label": session.label,
                    "parent": session.parent,
                    "resumed": true,
                    "fleet": fleet_outcome_json(&outcomes),
                }))),
                response => bail!("unexpected launch response: {response:?}"),
            }
        }

        /// What the caller calls its own session in the dashboard. Only the
        /// session itself may set it: the id comes from the environment this
        /// surface was launched with, never from the arguments, so an agent
        /// cannot rename somebody else's row.
        fn set_head_name(&self, arguments: &Value) -> Result<String> {
            let session_id = session_env("MUXLOOM_SESSION_ID")
                .context("set_head_name can only be called from within a muxloom session")?;
            const MAX_HEAD_NAME: usize = 80;
            let name: String = required_str(arguments, "name")?
                .trim()
                .chars()
                .filter(|c| !c.is_control())
                .collect();
            if name.is_empty() {
                bail!("name must not be empty");
            }
            if name.chars().count() > MAX_HEAD_NAME {
                bail!("name is too long: {MAX_HEAD_NAME} characters at most");
            }
            self.expect_ack(&DaemonRequest::SetLabel {
                session_id,
                label: name.clone(),
            })?;
            Ok(format!("Head name set to: {name}"))
        }

        /// This machine's captures searched in one round, and how many were
        /// passed over for being older than the near pool reaches.
        ///
        /// This surface negotiates no capabilities, so the fallback answers the
        /// version question instead of a handshake: a daemon too old for this
        /// says so, and gets walked session by session the old way.
        fn searched_sessions(
            &self,
            query: &str,
            deep: bool,
        ) -> Result<(Vec<SearchedSession>, usize)> {
            if let Ok((DaemonResponse::HistorySearch { hits, skipped, .. }, _)) =
                self.transact(&DaemonRequest::SearchHistoryAll {
                    query: query.into(),
                    max_matches: SEARCH_MAX_MATCHES,
                    deep,
                })
            {
                return Ok((
                    hits.into_iter()
                        .map(|hit| (hit.session_id, hit.label, hit.matches))
                        .collect(),
                    skipped,
                ));
            }
            let mut hits = Vec::new();
            for session in self
                .sessions()?
                .into_iter()
                .filter(|session| !session.temporary)
            {
                let Ok((DaemonResponse::HistoryMatches { matches }, _)) =
                    self.transact(&DaemonRequest::SearchHistory {
                        session_id: session.id.clone(),
                        query: query.into(),
                        max_matches: SEARCH_MAX_MATCHES,
                    })
                else {
                    continue;
                };
                hits.push((session.id, session.label, matches));
            }
            // A daemon this old has no pool to stop at: it read everything.
            Ok((hits, 0))
        }

        fn search_history(&self, arguments: &Value) -> Result<String> {
            let query = required_str(arguments, "query")?;
            let (hits, skipped) = match optional_str(arguments, "session_id") {
                Some(session_id) => match self.transact(&DaemonRequest::SearchHistory {
                    session_id: session_id.into(),
                    query: query.into(),
                    max_matches: SEARCH_MAX_MATCHES,
                }) {
                    // One named session is read whole however old it is: the
                    // pool exists to keep a search off captures nobody asked
                    // about, and this one was asked about.
                    Ok((DaemonResponse::HistoryMatches { matches }, _)) => {
                        (vec![(session_id.to_string(), String::new(), matches)], 0)
                    }
                    Ok(_) | Err(_) => (Vec::new(), 0),
                },
                None => self.searched_sessions(query, optional_bool(arguments, "deep"))?,
            };
            let mut results = Vec::new();
            // A search that stopped short says so. Coming back empty from a
            // pool that never opened the file reads exactly like a machine
            // where the word was never said.
            if skipped > 0 {
                results.push(json!({
                    "machine": crate::model::own_machine_name(),
                    "unsearched_sessions": skipped,
                    "note": format!(
                        "Searched the recently written captures; {skipped} older ones were not \
                         read. Only if this answer is genuinely not enough, ask again with \
                         deep=true - that reads every capture on the machine, which can run to \
                         gigabytes and take seconds.",
                    ),
                }));
            }
            for (session_id, label, matches) in hits {
                if matches.is_empty() {
                    continue;
                }
                results.push(json!({
                    "machine": crate::model::own_machine_name(),
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

        fn call(&self, name: &str, arguments: &Value) -> Result<String> {
            enforce_policy(&self.config.mcp, name)?;
            // What another machine holds is the controller's to answer. The
            // two that are only ever about other machines go straight out;
            // the rest go out only when they name one. A cross-machine WRITE
            // goes out too — the controller enforces the person's approval on
            // the far side — so it is offered here rather than refused.
            let elsewhere = self.elsewhere(arguments);
            if matches!(name, "search_conversations" | "read_conversation")
                || ((crate::relay::relayed(name) || crate::relay::approve_gated(name))
                    && elsewhere.is_some())
            {
                // The one argument that cannot be defaulted across the wire:
                // a launch here falls back to the caller's own folder, and
                // over there that folder is somebody else's or nobody's. Said
                // now rather than after a round trip and a person's approval.
                if name == "launch_session" {
                    if optional_str(arguments, "path").is_none() {
                        let machine = elsewhere.unwrap_or_default();
                        bail!(
                            "launch_session on {machine} needs an absolute `path` on that \
                             machine: the folder you are in is on this one. list_sessions {{ \
                             machine: \"{machine}\" }} shows where its agents already work, and \
                             list_directory {{ machine: \"{machine}\", path: \"...\" }} looks \
                             around."
                        );
                    }
                    // What this session may start, and hand on, is written in
                    // its environment here. Over there it is nowhere: the
                    // controller runs in no session. So it is weighed on this
                    // side and the answer travels with the call.
                    let own = own_powers();
                    check_may_launch(&own, agent_kind(arguments)?)?;
                    let mut relayed = arguments.clone();
                    stamp_powers(&mut relayed, &granted_powers(arguments, &own)?);
                    return self.relay(name, &relayed);
                }
                // Borrowing the controller's credentials does not widen what
                // this session may say to the person.
                if name == "send_channel_message" {
                    check_may_reach_person(&own_powers())?;
                }
                // How far this session may speak is written here too, and a
                // write crossing a machine is still it speaking. The chain
                // above the session it is aimed at lives over there, so the
                // list comes back first and the walk happens on this side,
                // where what counts as "our task" is known. A person on the far
                // machine approving the call is a separate question from this
                // one: they are saying yes to a stranger touching their box,
                // not lifting a limit the agent's own parent set.
                if let Some(target) = written_to(name, arguments) {
                    let own = own_powers();
                    if own.reach != Reach::Fleet && !reaches_without_records(&own, target) {
                        let machine = elsewhere.clone().unwrap_or_default();
                        let over_there =
                            self.relay("list_sessions", &json!({ "machine": machine }))?;
                        check_may_message(&own, target, &lineage_of_answer(&over_there))?;
                    }
                }
                return self.relay(name, arguments);
            }
            // The same weighing for this machine's own sessions, against the
            // records the daemon beside us holds.
            if let Some(target) = written_to(name, arguments) {
                self.check_reach(target)?;
            }
            match name {
                "list_machines" => self.list_machines(),
                "list_sessions" => {
                    let include_archived = optional_bool(arguments, "include_archived");
                    // Nobody asked for the archive: don't make the daemon
                    // gather it. What is still in the live list and archived
                    // was put down since the daemon started, and the filter
                    // below is what catches it.
                    let sessions = match include_archived {
                        true => self.sessions()?,
                        false => self.live_sessions()?,
                    };
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
                "send_channel_message" => self.send_channel_message(arguments),
                "send_input" => {
                    let session_id = required_str(arguments, "session_id")?;
                    // Only a running session can be typed into.
                    let kind = session_kind(self.live_session(session_id).ok().flatten());
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
                "set_head_name" => self.set_head_name(arguments),
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

    fn call(&self, _name: &str, _arguments: &Value) -> Result<String> {
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
                    channel: None,
                    channel_quote: None,
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
            channel: None,
            channel_quote: None,
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
            archived_at: None,
            pid: Some(1),
            dead: false,
            archived: false,
            recap: None,
            title: None,
            thread: None,
            seed: None,
            first_prompt: None,
            working,
            needs_attention: attention,
            attention_reason: attention.then(|| "waiting on a person".into()),
            composer: None,
            parent: None,
            powers: None,
            resumed_from: None,
            resumed_to: None,
        }
    }

    #[test]
    fn the_kind_a_keystroke_is_framed_for_is_read_off_one_record() {
        // Codex and Claude Code each decide whether an arriving return submits
        // or just breaks a line by how much came with it, so send_input has to
        // know which runtime it is typing into. It learned that by asking for
        // the roster and finding a line in it, which made every keystroke sent
        // to one session cost a drawing of every session on the machine.
        let mut claude = probe_session("muxloomd-claude-9-1-0", false, false);
        claude.kind = "claude".into();
        assert_eq!(session_kind(Some(claude)), Some(AgentKind::Claude));

        // A machine that cannot be asked, and a runtime this build has no name
        // for, are the same answer, and it changes nothing: typing raw is what
        // muxloom has always done.
        assert_eq!(session_kind(None), None);
        let mut stranger = probe_session("muxloomd-newthing-9-1-1", false, false);
        stranger.kind = "newthing".into();
        assert_eq!(session_kind(Some(stranger)), None);
    }

    #[test]
    fn a_wait_reads_the_archive_only_on_the_round_that_ends_it() {
        let filed = std::cell::Cell::new(0usize);
        let archive = || {
            filed.set(filed.get() + 1);
            let mut ended = probe_session("over", false, false);
            ended.dead = true;
            ended.archived = true;
            Ok(vec![ended])
        };

        // The rounds in the middle: the session is running, and neither the
        // rest of the machine nor its whole history of conversations is what
        // says so. Sixty of these go by in one wait.
        let running = waited_session(
            "runs",
            || Ok(Some(probe_session("runs", true, false))),
            archive,
        )
        .unwrap();
        assert_eq!(running.map(|session| session.id).as_deref(), Some("runs"));
        assert_eq!(
            filed.get(),
            0,
            "a running session was looked up in the archive"
        );

        // The round that ends it: nothing running holds the id, and the answer
        // still has to be able to say what the wait had been waiting on.
        let over = waited_session("over", || Ok(None), archive).unwrap();
        assert_eq!(over.map(|session| session.id).as_deref(), Some("over"));
        assert_eq!(filed.get(), 1);
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
                |_| None,
            )
            .unwrap(),
        )
        .unwrap();
        // Nothing on the board to read means nothing to report, and no advice
        // invented out of an empty exchange.
        assert_eq!(answer["waiting_on"], Value::Null);
        assert_eq!(answer["note"], Value::Null);

        let looked_up = std::cell::Cell::new(0usize);
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
                // Looked up by name: only the sessions actually holding an
                // unanswered message are asked about, and a wait that ends
                // empty with nothing outstanding asks about none of them.
                |session_id| {
                    looked_up.set(looked_up.get() + 1);
                    match session_id {
                        "thinking" => Some(probe_session("thinking", true, false)),
                        "gone-quiet" => Some(probe_session("gone-quiet", false, false)),
                        _ => None,
                    }
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
        assert_eq!(
            looked_up.get(),
            2,
            "one lookup per session actually being waited on, and the machine's list for none \
             of them"
        );
    }

    #[test]
    fn a_page_too_large_to_deliver_keeps_its_newest_and_says_what_it_dropped() {
        // A board carrying handovers between agents, which is what makes a
        // fifty-message page sixty thousand characters long.
        let long = "x".repeat(4 * 1024);
        let board: Vec<TalkMessage> = (1..=40)
            .map(|seq| direct(seq, "someone", "me", 1_000 + seq, &format!("{seq}:{long}")))
            .collect();
        let answer: Value = serde_json::from_str(
            &talk_wait(
                &json!({}),
                TalkFilter::default(),
                |_| {
                    Ok(TalkPage {
                        messages: board.clone(),
                        cursor: "c".into(),
                        truncated: false,
                    })
                },
                |_| None,
            )
            .unwrap(),
        )
        .unwrap();

        let messages = answer["messages"].as_array().unwrap();
        assert!(
            !messages.is_empty() && messages.len() < board.len(),
            "the page should be cut, not emptied and not passed through: {}",
            messages.len()
        );
        // Cut to the newest, because catching up is what the board is read
        // for. The oldest kept and everything after it are contiguous.
        let first = messages[0]["text"].as_str().unwrap();
        let last = messages[messages.len() - 1]["text"].as_str().unwrap();
        assert!(last.starts_with("40:"), "{last:.16}");
        let kept_from: u64 = first.split(':').next().unwrap().parse().unwrap();
        assert_eq!(kept_from as usize, 40 - messages.len() + 1);
        // Under the ceiling with room for one more message than fits, so the
        // cut is the ceiling's doing and not an off-by-one somewhere else.
        let weight: usize = messages.iter().map(|m| m.to_string().len()).sum();
        assert!(weight <= TALK_MAX_RESPONSE_CHARS, "{weight}");
        assert!(weight + long.len() > TALK_MAX_RESPONSE_CHARS, "{weight}");

        assert_eq!(answer["truncated"], json!(true));
        let note = answer["note"].as_str().unwrap();
        assert!(
            note.contains(&format!("{} of 40 messages", messages.len())),
            "{note}"
        );
        assert!(note.contains("before"), "{note}");

        // A page that already fits is handed back whole, untruncated, and
        // without a note inventing a problem it does not have.
        let small = vec![direct(1, "someone", "me", 1_000, "short enough")];
        let answer: Value = serde_json::from_str(
            &talk_wait(
                &json!({}),
                TalkFilter::default(),
                |_| {
                    Ok(TalkPage {
                        messages: small.clone(),
                        cursor: "c".into(),
                        truncated: false,
                    })
                },
                |_| None,
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(answer["messages"].as_array().unwrap().len(), 1);
        assert_eq!(answer["truncated"], json!(false));
        assert_eq!(answer["note"], Value::Null);
    }

    /// The ceiling is the second place a page can be cut, and it loses what the
    /// first one does if it cuts the same end. A caller following a cursor is
    /// given the oldest of what is new; trimming the newest off that and still
    /// handing back the board's own cursor would skip the middle, which is
    /// where an answer sits when it arrives ahead of the noise.
    #[test]
    fn a_page_too_large_for_a_cursor_keeps_its_oldest_and_holds_the_cursor_back() {
        let long = "x".repeat(4 * 1024);
        let board: Vec<TalkMessage> = (1..=40)
            .map(|seq| direct(seq, "someone", "me", 1_000 + seq, &format!("{seq}:{long}")))
            .collect();
        let origin = board[0].origin.clone();
        let filter = TalkFilter {
            since: TalkVector::from([(origin.clone(), 0)]),
            ..TalkFilter::default()
        };
        let answer: Value = serde_json::from_str(
            &talk_wait(
                &json!({}),
                filter,
                |_| {
                    Ok(TalkPage {
                        messages: board.clone(),
                        // What the board holds, which is what the old cursor
                        // was and what made this lose messages.
                        cursor: encode_cursor(&TalkVector::from([(origin.clone(), 40)])),
                        truncated: false,
                    })
                },
                |_| None,
            )
            .unwrap(),
        )
        .unwrap();

        let messages = answer["messages"].as_array().unwrap();
        assert!(
            !messages.is_empty() && messages.len() < board.len(),
            "the page should be cut, not emptied and not passed through: {}",
            messages.len()
        );
        let first = messages[0]["text"].as_str().unwrap();
        let last = messages[messages.len() - 1]["text"].as_str().unwrap();
        assert!(
            first.starts_with("1:"),
            "the oldest is the end kept: {first:.16}"
        );
        let kept_to: u64 = last.split(':').next().unwrap().parse().unwrap();
        assert_eq!(kept_to as usize, messages.len());

        // And the cursor stops on the last one handed over, so the next read
        // with it begins on the one after.
        let cursor = decode_cursor(answer["cursor"].as_str().unwrap());
        assert_eq!(
            cursor.get(&origin),
            Some(&kept_to),
            "the cursor may not reach past the page: {cursor:?}"
        );
        assert_eq!(answer["truncated"], json!(true));
        let note = answer["note"].as_str().unwrap();
        assert!(note.contains("oldest kept"), "{note}");
    }

    #[test]
    fn one_message_past_the_ceiling_is_still_handed_over() {
        // Cutting to fit must never cut to nothing: an empty page would read
        // as "nobody said anything", which is the one thing that is not true.
        let mut only = vec![direct(
            1,
            "someone",
            "me",
            1_000,
            &"y".repeat(TALK_MAX_RESPONSE_CHARS * 2),
        )];
        assert_eq!(
            fit_within(&mut only, TALK_MAX_RESPONSE_CHARS, Keep::Newest),
            1
        );
        assert_eq!(
            fit_within(&mut only, TALK_MAX_RESPONSE_CHARS, Keep::Oldest),
            1
        );

        // And a wait must decide on the untrimmed page: were it emptied
        // before the check, the caller would sit out the whole timeout on
        // messages already in hand.
        let board = vec![direct(
            1,
            "someone",
            "me",
            1_000,
            &"z".repeat(TALK_MAX_RESPONSE_CHARS * 2),
        )];
        let reads = std::cell::Cell::new(0usize);
        let answer: Value = serde_json::from_str(
            &talk_wait(
                &json!({ "wait_seconds": 30 }),
                TalkFilter::default(),
                |_| {
                    reads.set(reads.get() + 1);
                    Ok(TalkPage {
                        messages: board.clone(),
                        cursor: "c".into(),
                        truncated: false,
                    })
                },
                |_| None,
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(
            reads.get(),
            1,
            "it answered on the first look, not after waiting"
        );
        assert_eq!(answer["messages"].as_array().unwrap().len(), 1);
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
            &read_of("\x1b[1;32m❯ 1. Yes\x1b[m\n\x1b[m\x1b]0;claude\x07 2.\x1b[3CNo   \n\n"),
            true,
        );
        let (text, _) = page.split_once("\n\n[rows=").unwrap();
        assert_eq!(text, "❯ 1. Yes\n 2.   No");
    }

    /// A page as the daemon would answer it for `text`: the newest rows of
    /// a session read whole, with nothing above them.
    fn read_of(text: &str) -> ScreenRead {
        ScreenRead {
            text: text.trim_end().to_string(),
            offset_from_bottom: 0,
            total_rows: text.trim_end().lines().count(),
            reached_start: true,
            alternate_screen: false,
        }
    }

    #[test]
    fn a_page_says_how_many_rows_it_holds_and_where_the_next_one_starts() {
        // Five rows drawn, the newest two asked for: the trailer counts the
        // rows handed over, not the pane, and names the offset that reaches
        // the three above them.
        let page = screen_page(
            &ScreenRead {
                text: "four\nfive".into(),
                offset_from_bottom: 0,
                total_rows: 5,
                reached_start: true,
                alternate_screen: false,
            },
            true,
        );
        assert!(
            page.ends_with(
                "[rows=2 offset_from_bottom=0 older_history_above=true next_offset_from_bottom=2]"
            ),
            "{page}"
        );
        // The page that reaches the top says so, and offers no next.
        let top = screen_page(
            &ScreenRead {
                text: "one\ntwo\nthree".into(),
                offset_from_bottom: 2,
                total_rows: 5,
                reached_start: true,
                alternate_screen: false,
            },
            true,
        );
        assert!(
            top.ends_with("[rows=3 offset_from_bottom=2 older_history_above=false]"),
            "{top}"
        );
        // A read that did not reach the start of the log cannot vouch for the
        // top, whatever it counted.
        let deep = screen_page(
            &ScreenRead {
                text: "one\ntwo\nthree".into(),
                offset_from_bottom: 2,
                total_rows: 5,
                reached_start: false,
                alternate_screen: false,
            },
            true,
        );
        assert!(
            deep.contains("older_history_above=true next_offset_from_bottom=5]"),
            "{deep}"
        );
        // A blank page holds no rows, and a blank row folded away by the read
        // result is still a row the next page has to step over.
        let blank = screen_page(&read_of(""), true);
        assert!(blank.contains("[rows=0 "), "{blank}");
        let gappy = screen_page(&read_of("one\n\nthree"), false);
        assert!(gappy.contains("[rows=3 "), "{gappy}");
    }

    #[test]
    fn a_full_screen_program_is_named_and_sent_to_its_transcript() {
        let page = screen_page(
            &ScreenRead {
                text: "△ Permission required".into(),
                offset_from_bottom: 0,
                total_rows: 1,
                reached_start: true,
                alternate_screen: true,
            },
            false,
        );
        assert!(page.contains("older_history_above=false]"), "{page}");
        assert!(page.contains("[full-screen program:"), "{page}");
        assert!(page.contains("read_conversation"), "{page}");
        let inline = screen_page(&read_of("hello"), false);
        assert!(!inline.contains("full-screen"), "{inline}");
    }

    #[test]
    fn a_row_that_wrapped_reads_back_as_the_line_it_was() {
        // The renderer marks a row that ran off the right edge; the read
        // joins it to the row under it, in the raw grid and the read result
        // alike, and a row that merely ends at the edge stays its own line.
        let wrapped = "abc\x1b[m\x1b_wrap\x1b\\\ndef\x1b[m\nghi\x1b[m";
        assert_eq!(plain_screen(wrapped), "abcdef\nghi");
        let page = screen_page(&read_of(wrapped), false);
        let (text, trailer) = page.split_once("\n\n[rows=").unwrap();
        assert_eq!(text, "abcdef\nghi");
        // The rows the page holds are still the three the pane drew.
        assert!(trailer.starts_with("3 "), "{trailer}");
        // Blanks at the edge of a wrapped row are the line's own.
        assert_eq!(plain_screen("cd  \x1b_wrap\x1b\\\n/tmp\n"), "cd  /tmp");
        // A page ending on a wrapped row has nothing to join it to.
        assert_eq!(plain_screen("tail\x1b_wrap\x1b\\"), "tail");
    }

    #[test]
    fn read_result_strips_chrome_and_reads_content_in_order() {
        // Modeled on a real opencode start screen: block-art logo, a framed
        // composer, key hints, a transient tip, and a footer with the version.
        let plain = "\u{2584}\n\
                     \u{2588}\u{2588}\u{2588} \u{2588}\u{2588}\u{2588}\n\
                     \n\
                       \u{2503}\n\
                       \u{2503}  Ask anything... \"Fix broken tests\"\n\
                       \u{2503}\n\
                       \u{2503}  Build · Qwen3.8-27B (SGLang via SOIL) Qwen3.8-27B (SGLang via SOIL)\n\
                       \u{2579}\u{2580}\u{2580}\u{2580}\u{2580}\u{2580}\n\
                       tab agents  ctrl+p commands\n\
                     \n\
                             ● Tip Press ctrl+x h to toggle code block visibility in messages\n\
                     \n\
                      ~/Works/Terminal:main  ⊙ 1 MCP /status  1.18.23";
        assert_eq!(
            read_result(plain),
            "Ask anything... \"Fix broken tests\"\n\
             Build · Qwen3.8-27B (SGLang via SOIL) Qwen3.8-27B (SGLang via SOIL)\n\
             ● Tip Press ctrl+x h to toggle code block visibility in messages"
        );
    }

    #[test]
    fn read_result_drops_a_working_footer_with_usage_and_spinner() {
        // Bottom of a live session: answer text, a turn marker, the composer
        // frame's model line, and a footer with a usage figure.
        let plain = "Core fix: separated hull and turret rotation.\n\
                     hullYaw (A/D) controls where the tank faces and moves\n\
                     ▣ Build · Qwen3.8-27B (SGLang via SOIL) · 1m 23s\n\
                     Build · Qwen3.8-27B (SGLang via SOIL) ~/Works/Minimax-H3\n\
                     /Users/bytedance/Works/Minimax-H3  146.5K (56%)  ctrl+p commands  • OpenCode 1.18.23";
        assert_eq!(
            read_result(plain),
            "Core fix: separated hull and turret rotation.\n\
             hullYaw (A/D) controls where the tank faces and moves\n\
             ▣ Build · Qwen3.8-27B (SGLang via SOIL) · 1m 23s\n\
             Build · Qwen3.8-27B (SGLang via SOIL) ~/Works/Minimax-H3"
        );
    }

    #[test]
    fn read_result_drops_claude_codes_mode_bar_and_empty_prompt() {
        // The bottom of a working Claude Code session: the last answer, the
        // spinner line, the empty composer, and the mode bar under it. The
        // spinner line is what says a turn is running, so it stays; the empty
        // prompt and the mode bar are chrome.
        let plain = "⏺ Calling muxloom 2 times… (ctrl+o to expand)\n\
                     ✢ Forming… (9m 34s · ↓ 14.8k tokens · thinking with xhigh effort)\n\
                     ❯\n\
                     ⏵⏵ auto mode on (shift+tab to cycle) · esc to interrupt · ← for agents";
        assert_eq!(
            read_result(plain),
            "⏺ Calling muxloom 2 times… (ctrl+o to expand)\n\
             ✢ Forming… (9m 34s · ↓ 14.8k tokens · thinking with xhigh effort)"
        );
        // Idle, with a draft in the box: the draft is content.
        let idle = "⏺ Done.\n❯ run the tests again\n? for shortcuts";
        assert_eq!(read_result(idle), "⏺ Done.\n❯ run the tests again");
        // A prompt glyph starting a line of prose is not an empty prompt.
        assert!(!empty_prompt("> quoted reply"));
        assert!(empty_prompt("  ❯  "));
    }

    #[test]
    fn read_result_keeps_content_that_mentions_keys_or_versions() {
        // A real answer line that merely talks about keys or versions must
        // survive: the status-bar screen only runs on the last few rows.
        let plain = "To open the palette, press ctrl+p commands\n\
                     Shipped in release 1.18.23\n\
                     The tank now faces where it drives.";
        assert_eq!(
            read_result(plain),
            "To open the palette, press ctrl+p commands\n\
             Shipped in release 1.18.23\n\
             The tank now faces where it drives."
        );
    }

    #[test]
    fn read_screen_defaults_to_read_result_with_raw_opt_out() {
        let ansi = "  \u{2502}  Fix the flaky test in src/main.rs  \u{2502}\n\
                    \u{2579}\u{2580}\u{2580}\u{2580}\n\
                    ~/work:main  1.18.23\n";
        let raw = screen_page(&read_of(ansi), true);
        let (raw_text, _) = raw.split_once("\n\n[rows=").unwrap();
        assert_eq!(
            raw_text,
            "  \u{2502}  Fix the flaky test in src/main.rs  \u{2502}\n\
             \u{2579}\u{2580}\u{2580}\u{2580}\n\
             ~/work:main  1.18.23"
        );
        let read = screen_page(&read_of(ansi), false);
        let (read_text, _) = read.split_once("\n\n[rows=").unwrap();
        assert_eq!(read_text, "Fix the flaky test in src/main.rs");
    }

    #[test]
    fn daemon_and_controller_surfaces_share_tool_shapes() {
        let daemon: Vec<_> = specs(Flavor::Daemon);
        let controller: Vec<_> = specs(Flavor::Controller);
        for tool in &daemon {
            // The daemon runs inside one session and can name that row; the
            // controller is a fleet view with no row of its own.
            if tool.name == "set_head_name" {
                continue;
            }
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
                // Optional here only because the caller's own folder answers
                // for it, which is a local answer: naming another machine
                // takes the folder back off the surface's hands.
                assert!(tool.input_schema["properties"]["machine"].is_object());
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
                    crate::relay::relayed(tool.name) || crate::relay::approve_gated(tool.name),
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
        // A head name belongs to one session's row; only the daemon
        // surface, which lives inside a session, can set one.
        assert!(daemon.iter().any(|tool| tool.name == "set_head_name"));
        assert!(!controller.iter().any(|tool| tool.name == "set_head_name"));
    }

    /// Seeing a machine and being unable to work on it is the failure this
    /// guards: the daemon runs every relayable call already, and the only
    /// thing that ever stopped one was a schema with nowhere to write the
    /// machine's name down. So the rule is the allowlists', not a list kept
    /// by hand here — anything the controller will carry is addressable, and
    /// anything it will not must not look as though it is.
    #[test]
    fn a_daemon_can_name_a_machine_for_every_call_the_controller_will_carry() {
        let daemon = specs(Flavor::Daemon);
        for tool in &daemon {
            if !tool.input_schema["properties"]["machine"].is_object() {
                continue;
            }
            assert!(
                crate::relay::relayed(tool.name) || crate::relay::approve_gated(tool.name),
                "{} invites a machine the controller will not carry",
                tool.name
            );
        }
        // The exception on purpose: a watch that travelled would sit on this
        // machine waiting for something happening on another. It says so in
        // its own description instead of taking the argument.
        let wait_for = daemon.iter().find(|tool| tool.name == "wait_for").unwrap();
        assert!(!wait_for.input_schema["properties"]["machine"].is_object());
        assert!(wait_for.description.contains("watches this machine only"));
        // The whole point, spelled out: looking at another machine, and the
        // writes a person signs off on, are all reachable from inside a
        // session now.
        for name in [
            "read_screen",
            "send_input",
            "launch_session",
            "archive_session",
            "delete_session",
            "run_shell",
            "trigger",
            "list_files",
            "preview_file",
            "search_history",
        ] {
            let tool = daemon
                .iter()
                .find(|tool| tool.name == name)
                .unwrap_or_else(|| panic!("{name} missing from the daemon surface"));
            assert!(
                tool.input_schema["properties"]["machine"].is_object(),
                "{name} cannot be aimed at another machine"
            );
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
    fn reaching_the_human_is_offered_everywhere_and_names_no_machine() {
        for flavor in [Flavor::Controller, Flavor::Daemon] {
            let tool = specs(flavor)
                .into_iter()
                .find(|tool| tool.name == "send_channel_message")
                .expect("both surfaces can reach the human");
            // The one tool that does not take a `machine`, on either surface:
            // it ends up at a person's phone, and which machine dialled the
            // API is muxloom's business, not the caller's.
            assert!(
                tool.input_schema["properties"].get("machine").is_none(),
                "a channel message goes to a person, not to a machine"
            );
            assert_eq!(tool.input_schema["required"], json!(["text"]));
            // Answering means being able to say what you answer: the receipt
            // names the sent message on both surfaces, and `reply_to` accepts
            // that name back on both, so the loop closes however you reach it.
            assert!(
                tool.description.contains("message_id"),
                "{}",
                tool.description
            );
            assert_eq!(
                tool.input_schema["properties"]["reply_to"]["type"],
                json!("string"),
                "an answer must be able to name the message it answers"
            );
            // Both chats are named, and so is the difference between them: an
            // agent told "markdown renders" writes a table, and a table on a
            // phone that renders no markdown is the one thing this must not
            // quietly produce.
            assert!(tool.description.contains("WeChat"), "{}", tool.description);
            assert!(tool.description.contains("Lark"), "{}", tool.description);
            assert!(
                tool.description.contains("renders none of them"),
                "{}",
                tool.description
            );
            // The chat app and the talk board are separate surfaces: a channel
            // message posts nothing to the board, and posting on the board is
            // not how you answer a person on their phone.
            assert!(
                tool.description.contains("independent of the talk board"),
                "{}",
                tool.description
            );
        }
        // The board's post tool says the reverse side of the same line: writing
        // to the board never reaches a person's chat app.
        let board = specs(Flavor::Controller)
            .into_iter()
            .find(|tool| tool.name == "talk_post")
            .expect("the board is always offered");
        assert!(
            board
                .description
                .contains("never reaches a person's chat app"),
            "{}",
            board.description
        );
        // It says something on a machine's behalf without changing anything
        // there, which is exactly the shape of an errand a controller runs.
        assert!(crate::relay::relayed("send_channel_message"));
        // And speaking is acting, so read-only refuses it.
        assert!(WRITE_TOOLS.contains(&"send_channel_message"));
        assert!(instructions(Flavor::Daemon, &McpConfig::default()).contains("their phone"));
    }

    #[test]
    fn a_reply_to_travels_as_written_and_blank_is_no_answer() {
        assert_eq!(channel_reply_to(&json!({})), None);
        assert_eq!(channel_reply_to(&json!({ "reply_to": "" })), None);
        assert_eq!(channel_reply_to(&json!({ "reply_to": "  " })), None);
        assert_eq!(
            channel_reply_to(&json!({ "reply_to": " 7498971037873973384 " })),
            Some("7498971037873973384")
        );
        // Even a number the chat has never seen travels: the message simply
        // arrives unquoted, and refusing would lose the answer over a bubble.
        assert_eq!(channel_reply_to(&json!({ "reply_to": "1" })), Some("1"));
    }

    /// The board is a memory, and the only thing that keeps it one is what
    /// agents are told about it. Left to describe itself as a place to say what
    /// you are doing, it fills with status nobody will ever want again, and the
    /// notes worth keeping are what a busy read drops first - so the board
    /// stops being worth reading at exactly the moment it is being used most.
    #[test]
    fn the_board_is_offered_as_a_memory_and_a_post_defaults_to_a_note() {
        // Every draft names its own path. The default scope is "path", which
        // otherwise falls back to the calling session's directory - so a test
        // that left it out would pass on a developer's machine, where it is
        // running inside a muxloom session, and fail on any builder that is
        // not. What is under test here is `kind`, and it should not depend on
        // where the test happens to be run from.
        let posted = |mut arguments: Value| {
            arguments["path"] = json!("/work");
            talk_draft(&arguments, TalkAuthor::default())
                .expect("the draft is well formed")
                .kind
        };
        // An agent that says nothing about `kind` is writing something down.
        assert_eq!(
            posted(json!({ "text": "the retry lives in client.rs" })),
            TalkKind::Note
        );
        assert_eq!(
            posted(json!({ "text": "x", "kind": "note" })),
            TalkKind::Note
        );
        // What is not memory is refused rather than advised against, because
        // a board fills with passing remarks either way.
        let refused = |mut arguments: Value| {
            if arguments.get("path").is_none() {
                arguments["path"] = json!("/work");
            }
            talk_draft(&arguments, TalkAuthor::default())
                .expect_err("not something the board holds")
                .to_string()
        };
        // The other kind is a person speaking at the dashboard. It still
        // parses off the wire, where an older machine's posts arrive labelled
        // that way; it is not something a tool call may write.
        assert!(
            refused(json!({ "text": "x", "kind": "message" })).contains("person speaking"),
            "{}",
            refused(json!({ "text": "x", "kind": "message" }))
        );
        assert_eq!(TalkKind::parse("message").unwrap(), TalkKind::Message);
        // A direct message goes to a session, not onto a board.
        assert!(refused(json!({ "text": "x", "kind": "direct" })).contains("message_agent"));
        // And muxloom's own coordination paths are not anybody's memory.
        let reserved = refused(json!({
            "text": "x",
            "scope": "path",
            "path": "/muxloom/channel-leases",
        }));
        assert!(reserved.contains("muxloom's own"), "{reserved}");

        let tool = |name| {
            specs(Flavor::Controller)
                .into_iter()
                .find(|tool| tool.name == name)
                .expect("the board is always offered")
                .description
        };
        let post = tool("talk_post");
        assert!(post.contains("memory, not a transport"), "{post}");
        // The refusals are named, so an agent learns the rule from the tool
        // rather than from an error after the fact.
        assert!(
            post.contains("refused rather than advised against"),
            "{post}"
        );
        assert!(post.contains("set_head_name"), "{post}");
        assert!(post.contains("message_agent"), "{post}");
        // And the read side says the same, because an agent that thinks the
        // board is a feed will poll it however it was told to post.
        let read = tool("talk_read");
        assert!(read.contains("not worth polling"), "{read}");
        assert!(read.contains("list_sessions, not here"), "{read}");
        // Waiting still belongs to directs: that is where an answer arrives.
        assert!(read.contains("scope \"direct\" is the exception"), "{read}");
        for flavor in [Flavor::Controller, Flavor::Daemon] {
            let text = instructions(flavor, &McpConfig::default());
            assert!(text.contains("shared memory, not a chat"), "{text}");
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
        // controller. The list of errands is too long to recite now, so the
        // instructions state the rule instead: look and speak freely, and a
        // change over there waits on the person who owns the fleet.
        let daemon = instructions(Flavor::Daemon, &McpConfig::default());
        assert!(daemon.contains("`machine` argument"));
        assert!(daemon.contains("remote"));
        assert!(daemon.contains("the whole fleet is yours to work in"));
        assert!(daemon.contains("put to the person first"));
        assert!(daemon.contains("message_agent"));
        assert!(daemon.contains("not a reason to retry"));
    }

    /// OpenCode has no skill file, so this text is the whole of what it is
    /// told. A person writing in from their phone sees nothing at all until an
    /// agent sends something back, and an agent that starts working instead of
    /// answering is indistinguishable, from that end, from one that never
    /// heard them.
    #[test]
    fn an_agent_is_told_to_answer_the_person_before_it_starts_working() {
        for flavor in [Flavor::Controller, Flavor::Daemon] {
            let text = instructions(flavor, &McpConfig::default());
            assert!(text.contains("answer before you start the work"), "{text}");
        }
    }

    /// A list of tasks is a list of sessions. An agent that works them in a
    /// row spends four times the wall clock and gives the person watching one
    /// row that says whichever task it happens to be on.
    #[test]
    fn an_agent_holding_several_tasks_is_told_to_hand_them_out() {
        for flavor in [Flavor::Controller, Flavor::Daemon] {
            let text = instructions(flavor, &McpConfig::default());
            assert!(
                text.contains("more than one task is the signal to fan out"),
                "{text}"
            );
        }
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
            state: Mutex::new(state),
            state_path: root.join("state.json"),
        };
        (control, root)
    }

    #[test]
    fn controller_surface_gates_machines_on_the_enabled_set() {
        let (control, root) = controller_over_temp("gate");
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
        // Listed under its own name rather than `local` - and enabled, which
        // is read off the `local` key the state file still uses.
        assert_eq!(machine(crate::model::own_machine_name())["enabled"], true);
        assert_eq!(machine("gpu")["enabled"], false);

        control
            .state
            .lock()
            .unwrap()
            .enabled_hosts
            .insert("gpu".into());
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
    fn the_name_an_answer_hands_out_is_a_name_the_next_call_takes() {
        let (control, root) = controller_over_temp("named");
        let name = crate::model::own_machine_name();

        // Nothing here says `local` back at whoever asked: a session record
        // read on one machine and repeated on another has to name a machine,
        // and every node calls itself `local`.
        let listed: Value = serde_json::from_str(&control.list_machines().unwrap()).unwrap();
        assert_eq!(listed[0]["id"], name);
        let switched: Value = serde_json::from_str(
            &control
                .set_machine_enabled(&json!({ "machine": name, "enabled": true }))
                .unwrap(),
        )
        .unwrap();
        assert_eq!(switched["machine"], name);
        assert_eq!(switched["enabled_machines"], json!([name]));

        // And the word it replaced still addresses this machine, because the
        // config and the state file are written in it.
        assert_eq!(
            control.target(&json!({ "machine": name })).unwrap().id,
            "local"
        );
        assert_eq!(
            control.target(&json!({ "machine": "local" })).unwrap().id,
            "local"
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn ssh_writes_stay_inside_the_file_muxloom_owns() {
        let (control, root) = controller_over_temp("ssh");
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

    fn fleet_row(id: &str, parent: Option<&str>) -> DaemonSession {
        DaemonSession {
            id: id.into(),
            kind: "claude".into(),
            path: "/tmp".into(),
            label: id.into(),
            temporary: false,
            created_at: 1,
            archived_at: None,
            pid: None,
            dead: true,
            archived: true,
            recap: None,
            title: None,
            thread: None,
            seed: None,
            first_prompt: None,
            working: false,
            needs_attention: false,
            attention_reason: None,
            composer: None,
            parent: parent.map(str::to_string),
            powers: None,
            resumed_from: None,
            resumed_to: None,
        }
    }

    #[test]
    fn the_fleet_walk_stops_at_its_depth_and_total_bounds() {
        // A chain seven deep: the walk takes five levels and tells the
        // master the rest exist, it does not chase them.
        let mut chain = Vec::new();
        for depth in 0..8 {
            let id = format!("muxloomd-claude-{depth}-chain");
            let parent = (depth > 0).then(|| format!("muxloomd-claude-{}-chain", depth - 1));
            chain.push(fleet_row(&id, parent.as_deref()));
        }
        let (members, truncated) = fleet_resume_plan(&chain, "muxloomd-claude-0-chain");
        assert_eq!(members.len(), FLEET_RESUME_MAX_DEPTH);
        assert!(truncated);

        // Forty siblings under one master: the cap counts children, and the
        // remainder becomes a truncated line rather than a stampede.
        let mut wide = vec![fleet_row("muxloomd-claude-wide-master", None)];
        for sibling in 0..FLEET_RESUME_MAX_SESSIONS + 8 {
            wide.push(fleet_row(
                &format!("muxloomd-claude-{sibling}-wide"),
                Some("muxloomd-claude-wide-master"),
            ));
        }
        let (members, truncated) = fleet_resume_plan(&wide, "muxloomd-claude-wide-master");
        assert_eq!(members.len(), FLEET_RESUME_MAX_SESSIONS);
        assert!(truncated);

        // A live child is reported, never scheduled for relaunch; a
        // temporary one is only reported.
        let mut mixed = vec![
            fleet_row("muxloomd-claude-mixed-master", None),
            fleet_row(
                "muxloomd-claude-mixed-live",
                Some("muxloomd-claude-mixed-master"),
            ),
            fleet_row(
                "muxloomd-temp-mixed-scratch",
                Some("muxloomd-claude-mixed-master"),
            ),
        ];
        mixed[0].dead = false;
        mixed[0].archived = false;
        mixed[1].dead = false;
        mixed[1].archived = false;
        mixed[2].temporary = true;
        let (members, truncated) = fleet_resume_plan(&mixed, "muxloomd-claude-mixed-master");
        assert!(!truncated);
        assert_eq!(
            members
                .iter()
                .map(|member| (member.record.id.as_str(), member.action))
                .collect::<Vec<_>>(),
            vec![
                ("muxloomd-claude-mixed-live", FleetMemberAction::Running),
                ("muxloomd-temp-mixed-scratch", FleetMemberAction::Ephemeral),
            ]
        );
    }

    #[test]
    fn a_resume_by_muxloom_number_refuses_the_live_and_hands_over_the_archived() {
        let live = fleet_row("muxloomd-claude-target", None);
        let mut running = live.clone();
        running.dead = false;
        running.archived = false;
        let live_slice = std::slice::from_ref(&live);
        // An agent-native id passes the gate untouched.
        assert_eq!(
            fleet_resume_target(live_slice, "ses-native")
                .unwrap()
                .map(|record| record.id.clone()),
            None::<String>
        );
        // And passes it without the list being consulted at all, which is what
        // lets the caller decide off the id alone and not go asking the machine
        // for every session it has just to walk past the answer.
        assert!(fleet_resume_target(&[], "ses-native").unwrap().is_none());
        // The same number, still lived in: refused, never shadowed.
        let error = fleet_resume_target(&[running], "muxloomd-claude-target")
            .err()
            .unwrap()
            .to_string();
        assert!(error.contains("still live"), "{error}");
        // A number nobody ever had: refused with its name.
        let error = fleet_resume_target(live_slice, "muxloomd-claude-absent")
            .err()
            .unwrap()
            .to_string();
        assert!(error.contains("muxloomd-claude-absent"), "{error}");
        // Archived on this machine: the record, to come back as itself.
        let found = fleet_resume_target(live_slice, "muxloomd-claude-target")
            .unwrap()
            .map(|record| record.id.clone());
        assert_eq!(found, Some("muxloomd-claude-target".to_string()));
    }

    #[test]
    fn the_resume_caption_names_every_child_and_hands_back_the_exact_call() {
        let mut restored = fleet_row("muxloomd-claude-captioned", None);
        restored.thread = Some("ses-deep".into());
        let outcomes = vec![
            FleetOutcome {
                record: restored,
                status: "restored",
                resumed_with: Some("ses-deep".into()),
                detail: None,
            },
            FleetOutcome {
                record: fleet_row("muxloomd-claude-lost", None),
                status: "unresumed",
                resumed_with: None,
                detail: Some("no claude command configured".into()),
            },
        ];
        let caption =
            fleet_resume_caption("muxloomd-claude-master", "coordinator", &outcomes, false);
        assert!(
            caption.contains("Fleet resume report for coordinator"),
            "{caption}"
        );
        assert!(caption.contains("restored 1"), "{caption}");
        assert!(caption.contains("muxloomd-claude-captioned"), "{caption}");
        // The child that did not come back carries the call that can.
        assert!(
            caption.contains(
                "launch_session {\"kind\": \"claude\", \"resume_id\": \"muxloomd-claude-lost\""
            ),
            "{caption}"
        );
    }

    #[test]
    fn a_master_that_could_not_come_back_says_what_already_did() {
        let row = |id: &str, status: &'static str| FleetOutcome {
            record: fleet_row(id, None),
            status,
            resumed_with: None,
            detail: None,
        };
        let outcomes = vec![
            row("muxloomd-claude-restored", "restored"),
            row("muxloomd-claude-fresh", "fresh"),
            row("muxloomd-claude-lost", "unresumed"),
            row("muxloomd-claude-untouched", "running"),
        ];
        let said = fleet_already_back("muxloomd-claude-master", &outcomes)
            .expect("two children came back and the master did not");
        assert!(said.contains("muxloomd-claude-restored"), "{said}");
        assert!(said.contains("muxloomd-claude-fresh"), "{said}");
        // Only what this resume put back. One that never came back is not
        // running under anything, and one that never stopped was not this
        // resume's doing - naming either would send the caller after a
        // session that is not there or is not its concern.
        assert!(!said.contains("muxloomd-claude-lost"), "{said}");
        assert!(!said.contains("muxloomd-claude-untouched"), "{said}");
        // And the number to ask for to pick them up.
        assert!(said.contains("muxloomd-claude-master"), "{said}");

        // Nothing came back, so the failure is the whole of the story.
        let nothing = vec![row("muxloomd-claude-lost", "unresumed")];
        assert!(fleet_already_back("muxloomd-claude-master", &nothing).is_none());
    }

    #[test]
    fn a_relayed_caller_is_taken_only_when_the_environment_says_nothing() {
        // These rewrite a process-global; they run beside the daemon tests
        // that do the same, so they wait for the same lock.
        let _lock = daemon_env_lock();
        let _none = EnvScope::set("MUXLOOM_SESSION_ID", None);
        assert_eq!(
            relayed_caller(&json!({ "_muxloom_caller": "muxloomd-claude-3-1-0" })).as_deref(),
            Some("muxloomd-claude-3-1-0")
        );
        // A caller that is not a session id is not a caller.
        assert_eq!(
            relayed_caller(&json!({ "_muxloom_caller": "whoever says so" })),
            None
        );
        assert_eq!(relayed_caller(&json!({})), None);
        // The environment outranks an argument: an agent cannot rename its
        // parent by naming one, and neither can a relay.
        let _env = EnvScope::set("MUXLOOM_SESSION_ID", Some("muxloomd-claude-4-1-0"));
        assert_eq!(
            relayed_caller(&json!({ "_muxloom_caller": "muxloomd-claude-3-1-0" })).as_deref(),
            Some("muxloomd-claude-4-1-0")
        );
    }

    /// What an agent hands its subagent is what it holds, and no more — and
    /// what a person hands theirs is everything, because a person's agent
    /// answers to the person.
    #[test]
    fn a_launch_hands_on_no_more_than_the_session_making_it_holds() {
        let _lock = daemon_env_lock();
        let _id = EnvScope::set("MUXLOOM_SESSION_ID", None);
        let _kind = EnvScope::set("MUXLOOM_SESSION_KIND", None);
        let _reach = EnvScope::set("MUXLOOM_MAY_MESSAGE", None);
        let _launch = EnvScope::set("MUXLOOM_MAY_LAUNCH", None);
        let _person = EnvScope::set("MUXLOOM_MAY_REACH_PERSON", None);
        // Nobody's session: a person is launching, and what they start is
        // theirs. Not a subagent's defaults.
        assert_eq!(own_powers(), Powers::whole());
        assert_eq!(
            granted_powers(&json!({}), &own_powers()).unwrap(),
            Powers::whole()
        );

        // A claude session with the run of the fleet, starting a helper it
        // says nothing about: its own kind, its own task, and the person left
        // to it.
        let _id = EnvScope::set("MUXLOOM_SESSION_ID", Some("muxloomd-claude-1-1-0"));
        let _kind = EnvScope::set("MUXLOOM_SESSION_KIND", Some("claude"));
        let _reach = EnvScope::set("MUXLOOM_MAY_MESSAGE", Some("fleet"));
        let _launch = EnvScope::set("MUXLOOM_MAY_LAUNCH", Some("codex,claude,terminal"));
        let _person = EnvScope::set("MUXLOOM_MAY_REACH_PERSON", Some("yes"));
        let granted = granted_powers(&json!({}), &own_powers()).unwrap();
        assert_eq!(granted.reach, Reach::Task);
        assert_eq!(granted.launches, vec![AgentKind::Claude]);
        assert!(!granted.may_reach_person);

        // Asking for more than the parent holds gets the parent's answer:
        // opencode is not on its list, and neither dial goes past it.
        let asked = json!({
            "may_message": "fleet",
            "may_launch": ["codex", "opencode"],
            "may_reach_person": true,
        });
        let granted = granted_powers(&asked, &own_powers()).unwrap();
        assert_eq!(granted.reach, Reach::Fleet);
        assert_eq!(granted.launches, vec![AgentKind::Codex]);
        assert!(granted.may_reach_person);

        // And a parent that may not reach the person cannot hand that on,
        // however plainly the launch asks for it.
        let _person = EnvScope::set("MUXLOOM_MAY_REACH_PERSON", Some("no"));
        let _reach = EnvScope::set("MUXLOOM_MAY_MESSAGE", Some("task"));
        let granted = granted_powers(&asked, &own_powers()).unwrap();
        assert!(!granted.may_reach_person);
        assert_eq!(granted.reach, Reach::Task);

        // A refusal names who set the limit rather than a flag to pass.
        let error = check_may_launch(&own_powers(), AgentKind::OpenCode)
            .err()
            .unwrap()
            .to_string();
        assert!(error.contains("codex, claude, terminal"), "{error}");
        assert!(
            error.contains("The agent that started it set that"),
            "{error}"
        );
        let _launch = EnvScope::set("MUXLOOM_MAY_LAUNCH", Some(""));
        let error = check_may_launch(&own_powers(), AgentKind::Claude)
            .err()
            .unwrap()
            .to_string();
        assert!(error.contains("may not start others"), "{error}");
    }

    /// A launch aimed at another machine is weighed where the powers are
    /// legible and arrives holding the answer, because the controller running
    /// it lives in no session and has nothing to read.
    #[cfg(unix)]
    #[test]
    fn a_relayed_launch_carries_the_grant_its_own_machine_worked_out() {
        let _lock = daemon_env_lock();
        let granted = Powers {
            reach: Reach::Parent,
            launches: vec![AgentKind::Terminal],
            may_reach_person: false,
        };
        let mut relayed = json!({ "kind": "terminal", "path": "/works" });
        stamp_powers(&mut relayed, &granted);

        // Read on the controller, which is in no session.
        let _none = EnvScope::set("MUXLOOM_SESSION_ID", None);
        assert_eq!(relayed_powers(&relayed), Some(granted));
        assert_eq!(relayed_powers(&json!({})), None);

        // Never read on a machine where the caller has an environment of its
        // own: there the environment is the answer, and an argument saying
        // otherwise is an agent talking about itself.
        let _env = EnvScope::set("MUXLOOM_SESSION_ID", Some("muxloomd-claude-4-1-0"));
        assert_eq!(relayed_powers(&relayed), None);
    }

    /// A task-scoped session talks to its own team and is refused the rest —
    /// and the team is the whole tree under the task, not the siblings it
    /// happens to know about.
    #[cfg(unix)]
    #[test]
    fn a_task_scoped_session_reaches_its_own_work_and_stops_there() {
        let _lock = daemon_env_lock();
        let _id = EnvScope::set("MUXLOOM_SESSION_ID", Some("muxloomd-claude-lead"));
        let _root = EnvScope::set("MUXLOOM_TASK_ROOT", Some("muxloomd-claude-lead"));
        let _parent = EnvScope::set("MUXLOOM_SESSION_PARENT", None);
        let _reach = EnvScope::set("MUXLOOM_MAY_MESSAGE", Some("task"));
        let _launch = EnvScope::set("MUXLOOM_MAY_LAUNCH", Some("claude"));
        let _person = EnvScope::set("MUXLOOM_MAY_REACH_PERSON", Some("no"));
        let here = lineage(&[
            fleet_row("muxloomd-claude-lead", None),
            fleet_row("muxloomd-claude-hand", Some("muxloomd-claude-lead")),
            fleet_row("muxloomd-claude-grandchild", Some("muxloomd-claude-hand")),
            fleet_row("muxloomd-claude-stranger", None),
            fleet_row(
                "muxloomd-claude-someone-elses",
                Some("muxloomd-claude-stranger"),
            ),
        ]);
        let own = own_powers();
        for reachable in [
            "muxloomd-claude-lead",
            "muxloomd-claude-hand",
            "muxloomd-claude-grandchild",
        ] {
            check_may_message(&own, reachable, &here).unwrap();
        }
        let error = check_may_message(&own, "muxloomd-claude-someone-elses", &here)
            .err()
            .unwrap()
            .to_string();
        assert!(error.contains("not on this piece of work"), "{error}");
        assert!(error.contains("let it carry the message"), "{error}");
        // A session nobody on this machine has a record of is not this
        // session's team by default.
        assert!(check_may_message(&own, "muxloomd-claude-nowhere", &here).is_err());

        // A subagent started on another machine is one hop from this session
        // on that machine's own list, and that is enough to recognise it.
        let over_there = lineage_of_answer(
            &json!([
                { "session_id": "muxloomd-codex-far", "parent": "muxloomd-claude-lead" },
                { "session_id": "muxloomd-codex-native", "parent": null },
            ])
            .to_string(),
        );
        check_may_message(&own, "muxloomd-codex-far", &over_there).unwrap();
        assert!(check_may_message(&own, "muxloomd-codex-native", &over_there).is_err());

        // The full reach asks nothing of a lineage at all.
        let _reach = EnvScope::set("MUXLOOM_MAY_MESSAGE", Some("fleet"));
        check_may_message(&own_powers(), "muxloomd-claude-someone-elses", &[]).unwrap();

        // The narrowest answers to the one session that asked, and to whatever
        // it started to answer with, and to nothing else.
        let _reach = EnvScope::set("MUXLOOM_MAY_MESSAGE", Some("parent"));
        let _parent = EnvScope::set("MUXLOOM_SESSION_PARENT", Some("muxloomd-claude-boss"));
        check_may_message(&own_powers(), "muxloomd-claude-boss", &here).unwrap();
        let error = check_may_message(&own_powers(), "muxloomd-claude-stranger", &here)
            .err()
            .unwrap()
            .to_string();
        assert!(error.contains("and to nobody else"), "{error}");
        // Its own subtree, however far down: an agent that may start a helper
        // and may not speak to it is holding something it cannot use, and the
        // helper's questions upward would never be answerable.
        let _id = EnvScope::set("MUXLOOM_SESSION_ID", Some("muxloomd-claude-hand"));
        for mine in ["muxloomd-claude-grandchild", "muxloomd-claude-boss"] {
            check_may_message(&own_powers(), mine, &here).unwrap();
        }
        assert!(check_may_message(&own_powers(), "muxloomd-claude-lead", &here).is_err());
    }

    /// Fetching the records to weigh a reach is a round trip that hands back
    /// every conversation the machine has ever held — and across a relay, a
    /// round trip to another machine — before a keystroke goes anywhere. Most
    /// of what is asked does not turn on them.
    #[cfg(unix)]
    #[test]
    fn a_reach_settled_without_records_is_settled_the_same_way_with_them() {
        let _lock = daemon_env_lock();
        let _id = EnvScope::set("MUXLOOM_SESSION_ID", Some("muxloomd-claude-hand"));
        let _root = EnvScope::set("MUXLOOM_TASK_ROOT", Some("muxloomd-claude-lead"));
        let _parent = EnvScope::set("MUXLOOM_SESSION_PARENT", Some("muxloomd-claude-lead"));
        let _reach = EnvScope::set("MUXLOOM_MAY_MESSAGE", Some("task"));
        let _launch = EnvScope::set("MUXLOOM_MAY_LAUNCH", Some("claude"));
        let _person = EnvScope::set("MUXLOOM_MAY_REACH_PERSON", Some("no"));
        let here = lineage(&[
            fleet_row("muxloomd-claude-lead", None),
            fleet_row("muxloomd-claude-hand", Some("muxloomd-claude-lead")),
            fleet_row("muxloomd-claude-grandchild", Some("muxloomd-claude-hand")),
            fleet_row("muxloomd-claude-stranger", None),
        ]);
        let everyone = [
            "muxloomd-claude-lead",
            "muxloomd-claude-hand",
            "muxloomd-claude-grandchild",
            "muxloomd-claude-stranger",
            "muxloomd-claude-nowhere",
        ];

        // Reporting to the agent that started this one, and speaking to this
        // session itself, are recognised on the first hop of the walk, before
        // it has consulted anything.
        let own = own_powers();
        for close in ["muxloomd-claude-lead", "muxloomd-claude-hand"] {
            assert!(reaches_without_records(&own, close), "{close}");
        }
        // Anything further down is only reachable by following a parent, so
        // that one does send for the records — and is allowed once it has them.
        assert!(!reaches_without_records(&own, "muxloomd-claude-grandchild"));
        check_may_message(&own, "muxloomd-claude-grandchild", &here).unwrap();

        // The same split holds for the narrowest reach: the agent that asked is
        // named in the environment, and only the subtree needs looking up.
        let _reach = EnvScope::set("MUXLOOM_MAY_MESSAGE", Some("parent"));
        let narrow = own_powers();
        assert!(reaches_without_records(&narrow, "muxloomd-claude-lead"));
        assert!(!reaches_without_records(
            &narrow,
            "muxloomd-claude-grandchild"
        ));
        check_may_message(&narrow, "muxloomd-claude-grandchild", &here).unwrap();

        // Whatever is waved through with nothing in hand has to be waved
        // through with everything in hand as well, or the saving is a hole.
        for powers in [&own, &narrow] {
            for target in everyone {
                if reaches_without_records(powers, target) {
                    assert!(
                        check_may_message(powers, target, &here).is_ok(),
                        "{target} was let through with no records"
                    );
                }
            }
        }
        assert!(!reaches_without_records(&own, "muxloomd-claude-stranger"));

        // A process running in no session is outside nobody's work, and settles
        // that way rather than by recognising anyone: no records there either.
        let _reach = EnvScope::set("MUXLOOM_MAY_MESSAGE", Some("task"));
        let _root = EnvScope::set("MUXLOOM_TASK_ROOT", None);
        let _id = EnvScope::set("MUXLOOM_SESSION_ID", None);
        assert!(reaches_without_records(
            &own_powers(),
            "muxloomd-claude-stranger"
        ));
    }

    /// Every door into another agent's prompt box is the same door as far as
    /// the reach dial is concerned, or the narrow settings mean nothing.
    #[test]
    fn typing_into_a_session_counts_as_speaking_to_it() {
        let aimed = json!({ "session_id": "muxloomd-claude-hand", "text": "ping" });
        assert_eq!(
            written_to("message_agent", &aimed),
            Some("muxloomd-claude-hand")
        );
        assert_eq!(
            written_to("send_input", &aimed),
            Some("muxloomd-claude-hand")
        );

        // A trigger types on a pattern, which is a send_input with a delay on
        // it; a notify one raises a flag for a person and says nothing.
        let types = json!({
            "action": "set",
            "action_kind": "send_input",
            "session_id": "muxloomd-claude-hand",
            "pattern": "done",
            "text": "carry on",
        });
        assert_eq!(written_to("trigger", &types), Some("muxloomd-claude-hand"));
        let mut notifies = types.clone();
        notifies["action_kind"] = json!("notify");
        assert_eq!(written_to("trigger", &notifies), None);
        // The default action_kind is notify, and listing or deleting one is
        // not typing at all.
        let mut bare = types.clone();
        bare.as_object_mut().unwrap().remove("action_kind");
        assert_eq!(written_to("trigger", &bare), None);
        let mut listing = types.clone();
        listing["action"] = json!("list");
        assert_eq!(written_to("trigger", &listing), None);

        // Reading a session is not writing to it, and a call that names no
        // session is the tool's own to refuse.
        assert_eq!(written_to("read_screen", &aimed), None);
        assert_eq!(written_to("send_input", &json!({ "text": "ping" })), None);
    }

    /// The person hears about a piece of work from the agent they asked, not
    /// from every session working on it.
    #[test]
    fn a_session_not_handed_the_person_cannot_write_to_them() {
        let _lock = daemon_env_lock();
        let _reach = EnvScope::set("MUXLOOM_MAY_MESSAGE", Some("task"));
        let _launch = EnvScope::set("MUXLOOM_MAY_LAUNCH", Some("claude"));
        let _person = EnvScope::set("MUXLOOM_MAY_REACH_PERSON", Some("no"));
        let error = check_may_reach_person(&own_powers())
            .err()
            .unwrap()
            .to_string();
        assert!(
            error.contains("does not write to the person's chat"),
            "{error}"
        );
        assert!(error.contains("Tell that agent"), "{error}");

        let _person = EnvScope::set("MUXLOOM_MAY_REACH_PERSON", Some("yes"));
        check_may_reach_person(&own_powers()).unwrap();

        // A session nobody set limits on is a person's own agent.
        let _reach = EnvScope::set("MUXLOOM_MAY_MESSAGE", None);
        let _person = EnvScope::set("MUXLOOM_MAY_REACH_PERSON", None);
        check_may_reach_person(&own_powers()).unwrap();
    }

    /// The board is rooms, and the same dial says which of them a session may
    /// be heard in: its own task always, the folder it works in when it may
    /// talk to the others there, the machine and the world only at full reach.
    #[test]
    fn a_narrowed_session_posts_to_its_own_rooms_and_no_wider() {
        let _lock = daemon_env_lock();
        let task = TalkScope::Task {
            machine: String::new(),
            root_session: "muxloomd-claude-lead".into(),
        };
        let folder = TalkScope::Path {
            machine: String::new(),
            path: "/works/Terminal".into(),
        };
        let machine = TalkScope::Machine {
            machine: String::new(),
        };
        let _launch = EnvScope::set("MUXLOOM_MAY_LAUNCH", Some("claude"));
        let _person = EnvScope::set("MUXLOOM_MAY_REACH_PERSON", Some("no"));

        let _reach = EnvScope::set("MUXLOOM_MAY_MESSAGE", Some("task"));
        let own = own_powers();
        check_may_post(&own, &task).unwrap();
        check_may_post(&own, &folder).unwrap();
        let error = check_may_post(&own, &machine).err().unwrap().to_string();
        assert!(error.contains("scope \"task\" or \"path\""), "{error}");
        assert!(check_may_post(&own, &TalkScope::Global).is_err());

        let _reach = EnvScope::set("MUXLOOM_MAY_MESSAGE", Some("parent"));
        let own = own_powers();
        check_may_post(&own, &task).unwrap();
        let error = check_may_post(&own, &folder).err().unwrap().to_string();
        assert!(error.contains("its own task's board"), "{error}");

        // Nothing said about this session: a person's agent, and every room
        // is open to it.
        let _reach = EnvScope::set("MUXLOOM_MAY_MESSAGE", None);
        for scope in [task, folder, machine, TalkScope::Global] {
            check_may_post(&own_powers(), &scope).unwrap();
        }
    }

    /// A session is signed with what it is called now, not what it was named
    /// at launch — and never with its id.
    ///
    /// `MUXLOOM_SESSION_LABEL` is written into the keeper's environment once
    /// and never again, so every name acquired since — `set_head_name`, a
    /// person typing over it in the dashboard, the runtime's own title — was
    /// invisible to chat cards, board posts, and agent-to-agent envelopes.
    #[test]
    fn a_session_signs_with_the_name_it_answers_to_this_minute() {
        let _lock = daemon_env_lock();
        let _id = EnvScope::set("MUXLOOM_SESSION_ID", Some("muxloomd-claude-9-1-0"));
        let _label = EnvScope::set("MUXLOOM_SESSION_LABEL", Some("agent 3"));
        let _kind = EnvScope::set("MUXLOOM_SESSION_KIND", Some("claude"));
        let _path = EnvScope::set("MUXLOOM_SESSION_PATH", Some("/works/Terminal"));
        let _machine = EnvScope::set("MUXLOOM_MACHINE", Some("seed"));
        let _machine_label = EnvScope::set("MUXLOOM_MACHINE_LABEL", None);

        let mut renamed = fleet_row("muxloomd-claude-9-1-0", None);
        renamed.label = "channel routing".into();
        let table = [fleet_row("muxloomd-claude-8-1-0", None), renamed.clone()];

        // The lookup is handed the caller's own id, because that is the whole
        // question: what one session is called. It used to be read off a
        // listing of every session on the machine, gathered and classified to
        // have one line taken from it.
        let asked = std::cell::RefCell::new(Vec::new());
        let now = session_name_now(|id| {
            asked.borrow_mut().push(id.to_string());
            Ok(table.iter().find(|row| row.id == id).cloned())
        });
        assert_eq!(now.as_deref(), Some("channel routing"));
        assert_eq!(
            asked.into_inner(),
            vec!["muxloomd-claude-9-1-0".to_string()],
            "a session asked for a name other than its own"
        );
        assert_eq!(speaker(now), "channel routing · seed");

        // Nobody named it, but its runtime titled the conversation: that is
        // what the dashboard shows, so that is what the human should read.
        let mut titled = renamed.clone();
        titled.label = String::new();
        titled.title = Some("tracking down the resume leak".into());
        assert_eq!(
            session_name_now(|_| Ok(Some(titled))).as_deref(),
            Some("tracking down the resume leak")
        );

        // No lookup to be had: the launch-time name, then the folder. The id
        // is never one of the answers.
        assert_eq!(speaker(None), "agent 3 · seed");
        let _unnamed = EnvScope::set("MUXLOOM_SESSION_LABEL", None);
        assert_eq!(speaker(None), "Terminal · seed");
        let _nowhere = EnvScope::set("MUXLOOM_SESSION_PATH", None);
        assert_eq!(speaker(None), "claude · seed");
    }

    /// The same process-global the daemon-surface tests rewrite; the gate
    /// runs them one thread at a time, and this keeps them honest if that
    /// ever changes.
    fn daemon_env_lock() -> std::sync::MutexGuard<'static, ()> {
        static ENVELOPE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        ENVELOPE_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Set an environment variable for the duration of a test; the same
    /// discipline the daemon-surface tests use, at module scope.
    struct EnvScope {
        key: String,
        previous: Option<String>,
    }

    impl EnvScope {
        fn set(key: &str, value: Option<&str>) -> Self {
            let scope = Self {
                key: key.into(),
                previous: std::env::var(key).ok(),
            };
            match value {
                Some(value) => unsafe { std::env::set_var(key, value) },
                None => unsafe { std::env::remove_var(key) },
            }
            scope
        }
    }

    impl Drop for EnvScope {
        fn drop(&mut self) {
            match &self.previous {
                Some(value) => unsafe { std::env::set_var(&self.key, value) },
                None => unsafe { std::env::remove_var(&self.key) },
            }
        }
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
        fn a_default_listing_still_leaves_out_a_session_that_was_put_down() {
            let mut surface = surface("listing-put-down");
            let workdir = std::env::temp_dir();
            let launched = call(
                &mut surface,
                "launch_session",
                json!({ "kind": "terminal", "path": workdir.to_str().unwrap() }),
            );
            let launched: Value = serde_json::from_str(&launched).unwrap();
            let session_id = launched["session_id"].as_str().unwrap().to_string();
            call(
                &mut surface,
                "archive_session",
                json!({ "session_id": session_id }),
            );

            // A default listing asks the daemon for what it is running, which
            // is not the same as what is still going: a session put down under
            // this daemon stays in that map until a later generation retires
            // it, so leaving it out is this side's job either way.
            let listed = call(&mut surface, "list_sessions", json!({}));
            assert!(
                !listed.contains(&session_id),
                "an archived session is not what is going on: {listed}"
            );
            let all = call(
                &mut surface,
                "list_sessions",
                json!({ "include_archived": true }),
            );
            assert!(
                all.contains(&session_id),
                "and asked for, it must still be there: {all}"
            );
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

        // The tests below rewrite MUXLOOM_SESSION_ID, a process-global env var
        // the whole binary reads, so they must not overlap one another.
        static HEAD_NAME_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

        /// Hold one env var at a value for a test, restoring it on drop.
        struct EnvScope {
            key: &'static str,
            previous: Option<String>,
        }
        impl EnvScope {
            fn set(key: &'static str, value: Option<&str>) -> Self {
                let previous = std::env::var(key).ok();
                match value {
                    Some(value) => unsafe { std::env::set_var(key, value) },
                    None => unsafe { std::env::remove_var(key) },
                }
                Self { key, previous }
            }
        }
        impl Drop for EnvScope {
            fn drop(&mut self) {
                match &self.previous {
                    Some(value) => unsafe { std::env::set_var(self.key, value) },
                    None => unsafe { std::env::remove_var(self.key) },
                }
            }
        }

        #[test]
        fn set_head_name_renames_the_callers_own_session_row() {
            let _env_lock = HEAD_NAME_ENV_LOCK
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let mut surface = surface("headname");
            let launched: Value = serde_json::from_str(&call(
                &mut surface,
                "launch_session",
                json!({ "kind": "terminal", "path": std::env::temp_dir().to_str().unwrap() }),
            ))
            .unwrap();
            let session_id = launched["session_id"].as_str().unwrap().to_string();

            // The id comes from the environment this surface was launched in,
            // never from the arguments, so set it for the call and clear it after.
            let scope = EnvScope::set("MUXLOOM_SESSION_ID", Some(&session_id));
            let reply = call(
                &mut surface,
                "set_head_name",
                json!({ "name": "  fixing the lexer \u{7f} " }),
            );
            drop(scope);
            assert!(
                reply.contains("Head name set to: fixing the lexer"),
                "{reply}"
            );

            // The row the dashboard reads now carries the new head name.
            let listed: Value =
                serde_json::from_str(&call(&mut surface, "list_sessions", json!({}))).unwrap();
            let row = listed
                .as_array()
                .unwrap()
                .iter()
                .find(|session| session["session_id"] == session_id)
                .expect("session must still be listed");
            assert_eq!(row["label"], "fixing the lexer");

            call(
                &mut surface,
                "delete_session",
                json!({ "session_id": session_id }),
            );
        }

        #[test]
        fn set_head_name_is_refused_outside_a_session_and_on_a_bad_name() {
            let _env_lock = HEAD_NAME_ENV_LOCK
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let surface = surface("headname-none");

            // No session id in the environment: this surface has no row to name.
            let scope = EnvScope::set("MUXLOOM_SESSION_ID", None);
            let error = surface
                .call("set_head_name", &json!({ "name": "x" }))
                .unwrap_err()
                .to_string();
            drop(scope);
            assert!(
                error.contains("only be called from within a muxloom session"),
                "{error}"
            );

            // A name that is only control characters or whitespace clears to
            // empty and is refused rather than stored blank.
            let scope = EnvScope::set("MUXLOOM_SESSION_ID", Some("any-session"));
            let error = surface
                .call("set_head_name", &json!({ "name": "  \u{1f} " }))
                .unwrap_err()
                .to_string();
            assert!(error.contains("must not be empty"), "{error}");

            // And a name past the cap is refused before it reaches the daemon.
            let error = surface
                .call("set_head_name", &json!({ "name": "x".repeat(81) }))
                .unwrap_err()
                .to_string();
            drop(scope);
            assert!(error.contains("too long"), "{error}");
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
                json!({ "text": "the kettle boils dry above 4000m", "path": here }),
            ))
            .unwrap();
            assert_eq!(posted["scope"], "path");
            assert_eq!(posted["scope_path"], here);
            // Nothing said what kind this was, and the board is a memory: what
            // an agent writes on it is a note unless it says otherwise.
            assert_eq!(posted["kind"], "note");
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
                [
                    "the kettle boils dry above 4000m",
                    "the flour is in the second drawer"
                ],
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
                json!({ "text": "the kettle scales up within a week here", "path": here }),
            );
            let after = read(
                &mut surface,
                json!({ "path": here, "since_cursor": cursor }),
            );
            assert_eq!(texts(&after), ["the kettle scales up within a week here"]);

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
                        who: String::new(),
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
                            who: String::new(),
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

        /// Seeing a machine and being unable to work on it was the whole
        /// complaint. A call that names another machine has to leave this one
        /// — including the ones that change something over there, which the
        /// person approves on the far side rather than this end refusing.
        #[test]
        fn a_call_aimed_at_another_machine_leaves_this_one() {
            let surface = surface("aimed");
            let away = json!({ "machine": "somewhere-else" });
            let aimed = |extra: Value| {
                let mut arguments = away.clone();
                for (key, value) in extra.as_object().unwrap() {
                    arguments[key] = value.clone();
                }
                arguments
            };
            // Nothing is watching, so "there is no controller" is the answer
            // to every one of these — and it is the answer only because the
            // call went looking for one instead of being served from here.
            for (tool, arguments) in [
                (
                    "read_screen",
                    aimed(json!({ "session_id": "muxloomd-x-1-1" })),
                ),
                (
                    "send_input",
                    aimed(json!({ "session_id": "muxloomd-x-1-1", "text": "hello" })),
                ),
                (
                    "launch_session",
                    aimed(json!({ "kind": "claude", "path": "/srv/work" })),
                ),
                (
                    "archive_session",
                    aimed(json!({ "session_id": "muxloomd-x-1-1" })),
                ),
                ("run_shell", aimed(json!({ "script": "uptime" }))),
                ("list_files", aimed(json!({ "path": "/srv" }))),
            ] {
                let error = surface.call(tool, &arguments).unwrap_err().to_string();
                assert!(
                    error.contains("attached muxloom controller"),
                    "{tool} was answered here: {error}"
                );
            }

            // A launch is the one call whose missing argument this machine
            // used to fill in, and its folder means nothing over there. Said
            // before the errand goes out, not after a person has approved it.
            let error = surface
                .call("launch_session", &aimed(json!({ "kind": "claude" })))
                .unwrap_err()
                .to_string();
            assert!(error.contains("needs an absolute `path`"), "{error}");
            assert!(error.contains("somewhere-else"), "{error}");

            // Aimed at this machine, nothing has changed: the call is served
            // here, and fails on its own merits rather than for want of a
            // controller.
            let error = surface
                .call(
                    "read_screen",
                    &json!({ "machine": "local", "session_id": "muxloomd-x-1-1" }),
                )
                .unwrap_err()
                .to_string();
            assert!(!error.contains("attached muxloom controller"), "{error}");
        }

        #[test]
        fn the_fleet_an_agent_sees_is_the_one_a_controller_came_round_and_named() {
            let (mut surface, paths) = surface_and_paths("reach", Config::default());

            // A controller comes round and says where it can reach, which is
            // the only way this daemon ever learns another machine exists.
            let poll = DaemonRequest::RelayPoll {
                who: String::new(),
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

            // This machine answers as itself, under its own name rather than
            // `local` — which out here is the machine the asking agent is
            // sitting on — and carries the label the fleet knows it by too.
            assert_eq!(machines[0]["id"], crate::model::own_machine_name());
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

        /// A stand-in for Claude Code that survives a `--resume <id>` riding
        /// onto its command line: sh runs the script and passes the extra
        /// words as positional arguments the script never reads. It echoes
        /// each line it is handed so a test can read back what was typed.
        fn fake_claude_script(name: &str) -> PathBuf {
            let script = std::env::temp_dir().join(format!(
                "mxl-{name}-{}-{}.sh",
                std::process::id(),
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .subsec_nanos()
            ));
            std::fs::write(
                &script,
                "rule='────────────────────────────────────────'\n\
                 draw() { printf '%s\\n❯ \\n%s\\n' \"$rule\" \"$rule\"; }\n\
                 draw\n\
                 while IFS= read -r line; do\n\
                 \x20 printf '%s\\n' \"$line\"\n\
                 \x20 draw\n\
                 done\n",
            )
            .unwrap();
            script
        }

        fn claude_config(script: &std::path::Path) -> Config {
            let mut config = Config::default();
            config.agents.claude.command = "sh".into();
            config.agents.claude.args = vec![script.to_str().unwrap().to_string()];
            config
        }

        #[test]
        fn resuming_a_coordinator_by_its_muxloom_id_comes_back_with_its_fleet() {
            let _env_lock = HEAD_NAME_ENV_LOCK
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let script = fake_claude_script("fleet-resume");
            let config = claude_config(&script);
            let (mut surface, paths) = surface_and_paths("fleet-resume", config);
            let workdir = std::env::temp_dir().to_str().unwrap().to_string();
            let launched = |surface: &mut DaemonControl, arguments: Value| -> Value {
                serde_json::from_str(&call(surface, "launch_session", arguments)).unwrap()
            };

            // A coordinator and two children: one child carries a native
            // conversation, the other never matched one.
            let _no_caller = EnvScope::set("MUXLOOM_SESSION_ID", None);
            let master = launched(
                &mut surface,
                json!({ "kind": "claude", "path": workdir, "label": "coordinator",
                        "resume_id": "ses-master-native" }),
            );
            let master_id = master["session_id"].as_str().unwrap().to_string();
            assert!(master["parent"].is_null(), "{master}");
            let _caller = EnvScope::set("MUXLOOM_SESSION_ID", Some(&master_id));
            let first = launched(
                &mut surface,
                json!({ "kind": "claude", "path": workdir, "label": "child-one",
                        "resume_id": "ses-child-one" }),
            );
            let second = launched(
                &mut surface,
                json!({ "kind": "claude", "path": workdir, "label": "child-two" }),
            );
            let first_id = first["session_id"].as_str().unwrap().to_string();
            let second_id = second["session_id"].as_str().unwrap().to_string();
            assert_eq!(first["parent"], json!(master_id));
            assert_eq!(second["parent"], json!(master_id));

            // The fleet dies: archive leaves the records and the parent
            // links behind, which is the whole account that outlives a master.
            for id in [&first_id, &second_id, &master_id] {
                call(&mut surface, "archive_session", json!({ "session_id": id }));
            }

            // The master comes back asking only to be itself: same number,
            // same label, and both children back on their own numbers.
            let resumed = launched(
                &mut surface,
                json!({ "kind": "claude", "path": workdir, "resume_id": master_id }),
            );
            assert_eq!(resumed["session_id"], json!(master_id));
            assert_eq!(resumed["label"], json!("coordinator"));
            assert_eq!(resumed["resumed"], json!(true));
            let status_of = |wanted: &str| {
                resumed["fleet"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|member| member["old_session_id"] == json!(wanted))
                    .unwrap()["status"]
                    .clone()
            };
            assert_eq!(status_of(&first_id), json!("restored"));
            assert_eq!(status_of(&first_id), json!("restored"));
            assert_eq!(status_of(&second_id), json!("fresh"));

            let listed: Value =
                serde_json::from_str(&call(&mut surface, "list_sessions", json!({}))).unwrap();
            for (id, label) in [
                (master_id.as_str(), "coordinator"),
                (first_id.as_str(), "child-one"),
                (second_id.as_str(), "child-two"),
            ] {
                let row = listed
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|entry| entry["session_id"] == json!(id))
                    .unwrap_or_else(|| panic!("{id} never came back: {listed:#}"));
                assert_eq!(row["label"], json!(label));
                assert_eq!(row["archived"], json!(false));
            }
            let child_row = listed
                .as_array()
                .unwrap()
                .iter()
                .find(|entry| entry["session_id"] == json!(first_id))
                .unwrap();
            assert_eq!(child_row["parent"], json!(master_id));

            // The master's first turn carries the caption: queued into the
            // outbox as the launch's seed, it is either still waiting there
            // or already typed onto the screen this moment.
            let deadline = Instant::now() + Duration::from_secs(20);
            let captioned = loop {
                let queued = std::fs::read_to_string(&paths.outbox).unwrap_or_default();
                if queued.contains("Fleet resume report") && queued.contains(&first_id) {
                    break true;
                }
                let screen = call(
                    &mut surface,
                    "read_screen",
                    json!({ "session_id": master_id, "lines": 80 }),
                );
                if screen.contains("Fleet resume report") && screen.contains(&first_id) {
                    break true;
                }
                assert!(Instant::now() < deadline, "the caption never arrived");
                thread::sleep(Duration::from_millis(100));
            };
            assert!(captioned);
        }

        #[test]
        fn a_resume_by_muxloom_id_refuses_the_session_that_is_still_live() {
            let _env_lock = HEAD_NAME_ENV_LOCK
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let script = fake_claude_script("fleet-alive");
            let mut surface = surface_with("fleet-alive", claude_config(&script));
            let workdir = std::env::temp_dir().to_str().unwrap().to_string();
            let _no_caller = EnvScope::set("MUXLOOM_SESSION_ID", None);
            let launched: Value = serde_json::from_str(&call(
                &mut surface,
                "launch_session",
                json!({ "kind": "claude", "path": workdir, "label": "running" }),
            ))
            .unwrap();
            let id = launched["session_id"].as_str().unwrap().to_string();
            let error = surface
                .call(
                    "launch_session",
                    &json!({ "kind": "claude", "path": workdir, "resume_id": id }),
                )
                .unwrap_err()
                .to_string();
            assert!(error.contains("still live"), "{error}");
            let listed: Value =
                serde_json::from_str(&call(&mut surface, "list_sessions", json!({}))).unwrap();
            let holders = listed
                .as_array()
                .unwrap()
                .iter()
                .filter(|entry| entry["session_id"] == json!(id))
                .count();
            assert_eq!(holders, 1);
        }

        #[test]
        fn closing_a_master_closes_every_session_under_it() {
            let _env_lock = HEAD_NAME_ENV_LOCK
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let script = fake_claude_script("fleet-close");
            let mut surface = surface_with("fleet-close", claude_config(&script));
            let workdir = std::env::temp_dir().to_str().unwrap().to_string();
            let launched = |surface: &mut DaemonControl, caller: Option<&str>, label: &str| {
                let _caller = EnvScope::set("MUXLOOM_SESSION_ID", caller);
                let started: Value = serde_json::from_str(&call(
                    surface,
                    "launch_session",
                    json!({ "kind": "claude", "path": workdir, "label": label }),
                ))
                .unwrap();
                started["session_id"].as_str().unwrap().to_string()
            };

            // A master, a subagent under it, one under that, and a session
            // that has nothing to do with any of them.
            let master = launched(&mut surface, None, "master");
            let child = launched(&mut surface, Some(&master), "child");
            let grandchild = launched(&mut surface, Some(&child), "grandchild");
            let bystander = launched(&mut surface, None, "bystander");

            call(
                &mut surface,
                "archive_session",
                json!({ "session_id": master }),
            );

            let listed: Value = serde_json::from_str(&call(
                &mut surface,
                "list_sessions",
                json!({ "include_archived": true }),
            ))
            .unwrap();
            let closed = |id: &str| -> bool {
                listed
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|entry| entry["session_id"] == json!(id))
                    .unwrap_or_else(|| panic!("{id} is not listed: {listed:#}"))["archived"]
                    == json!(true)
            };
            assert!(closed(&master), "the master itself is closed");
            assert!(
                closed(&child),
                "a subagent has nobody to report to once its master is closed: {listed:#}"
            );
            assert!(
                closed(&grandchild),
                "the walk reaches every level, not just the first: {listed:#}"
            );
            assert!(
                !closed(&bystander),
                "a session outside the fleet is left alone: {listed:#}"
            );
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
            // It says what it is doing the way the real one does — through
            // the terminal title, a spinner glyph at its head for the whole
            // of a turn and `✳` once the turn ends — and repaints the whole
            // screen each time.
            std::fs::write(
                &script,
                "rule='────────────────────────────────────────'\n\
                 title='◐ Claude Code'\n\
                 said=''\n\
                 draw() { printf '\\033]0;%s\\007\\033[2J\\033[H%s\\n%s\\n❯ \\n%s\\n' \\\n\
                 \x20 \"$title\" \"$said\" \"$rule\" \"$rule\"; }\n\
                 draw\n\
                 while IFS= read -r line; do\n\
                 \x20 said=\"$said\n\
                 $line\"\n\
                 \x20 case $line in\n\
                 \x20   *settled*) title='✳ Claude Code' ;;\n\
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

        #[test]
        fn with_no_channel_bound_the_agent_is_told_who_can_bind_one() {
            let surface = surface("channel");
            // Nothing is bound here and no controller is watching, so the
            // errand cannot be handed off either. What comes back has to be
            // the useful half of that — where a human sets a channel up —
            // and not a complaint about a relay the agent cannot act on.
            let error = surface
                .call(
                    "send_channel_message",
                    &json!({ "text": "the run is done" }),
                )
                .expect_err("nothing is bound to send through")
                .to_string();
            assert!(error.contains("press c"), "{error}");
            assert!(!error.contains("controller"), "{error}");
        }
    }
}
