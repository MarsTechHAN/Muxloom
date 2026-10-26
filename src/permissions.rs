//! Who may do what, decided in one place.
//!
//! Every call either control surface takes is weighed here, once, before it
//! goes anywhere — before a daemon decides whether to answer it itself or hand
//! it to a controller for another machine. A rule that lived inside one tool,
//! or on one of the two routes a call can take, was a rule an agent stepped
//! past by naming a machine; so nothing about who may do what is decided
//! anywhere else.
//!
//! Three questions, always in this order:
//!
//! 1. **Who is asking.** What a session may do was written into its
//!    environment by the launch that started it (`MUXLOOM_MAY_*`, see
//!    [`own_powers`]), and is never taken from an argument: an agent describing
//!    its own limits could only describe them wrong. A session nobody started
//!    carries none of it and holds everything, because a person's own agent
//!    answers to the person.
//! 2. **What the call does, and to whom** ([`Act`]). Each limit is written
//!    against an act rather than a tool, so a tool added later is weighed the
//!    day it exists:
//!    - looking is free;
//!    - speaking to another agent is the reach dial (`may_message`), on every
//!      door into a prompt box ([`written_to`]);
//!    - speaking to the person is `may_reach_person`;
//!    - writing on the board is the reach dial again, read as which rooms;
//!    - starting a session is `may_launch`, and on the daemon surface the
//!      caller's own folder;
//!    - closing a session, or bringing an archived one back, is a session's
//!      own subtree for anything another agent started ([`check_owns`]).
//! 3. **Where it lands** ([`Crossing`]). On the caller's own machine the
//!    answers above are the whole of it. On another, looking and speaking
//!    travel through the controller as they are; a change waits for the
//!    person's approval; and some things never travel at all.
//!
//! The approval is the one question nothing here can answer, because only the
//! person can. Where to put it to them is the asking agent's call — it knows
//! whether the work was typed into its terminal at the dashboard or arrived
//! from a phone — and `approvals::AskVia` carries that judgement to the
//! controller that asks.
//!
//! A moderator is not an exception to any of this. It is a session a person
//! started in a folder muxloom owns, and it holds everything because a person
//! started it, not because of where it lives: a session another agent puts in
//! that folder keeps the limits it was launched with, and is not handed the
//! fleet-wide surface at all (see `mcp_register::handover_to_controller`).

use std::env;

use anyhow::{Result, anyhow, bail};
use serde_json::Value;

#[cfg(unix)]
use crate::daemon_protocol::DaemonSession;
use crate::{
    control::optional_str,
    model::{AgentKind, Powers, Reach},
};

/// What a call does, which is what every rule here is written against.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Act {
    /// Reports something and changes nothing.
    Look,
    /// Puts words in front of another agent.
    Speak,
    /// Puts words in front of the person, on their phone.
    TellPerson,
    /// Writes on the talk board.
    Post,
    /// Starts a session, or brings an archived one back.
    Start,
    /// Archives or deletes a session, and everything it started.
    Close,
    /// Runs something on the machine: a shell, a standing trigger.
    Command,
    /// Changes which machines exist and how they are reached.
    Rewire,
    /// Renames the caller's own row.
    NameSelf,
}

/// What `tool` does. A name this build has never heard of is counted as a
/// command — the reading that refuses most — so a newer client's tool is never
/// waved through a read-only machine for want of a line here.
pub fn act(tool: &str) -> Act {
    match tool {
        "list_machines"
        | "list_sessions"
        | "read_screen"
        | "wait_for"
        | "talk_read"
        | "list_resume_candidates"
        | "search_history"
        | "search_conversations"
        | "read_conversation"
        | "list_directory"
        | "list_files"
        | "search_files"
        | "preview_file" => Act::Look,
        "message_agent" | "send_input" => Act::Speak,
        "send_channel_message" => Act::TellPerson,
        "talk_post" => Act::Post,
        "launch_session" => Act::Start,
        "archive_session" | "delete_session" => Act::Close,
        "set_machine_enabled" | "ssh_host" => Act::Rewire,
        "set_head_name" => Act::NameSelf,
        _ => Act::Command,
    }
}

/// Whether a tool changes anything, which is what `mcp.read_only` denies.
pub fn changes_something(tool: &str) -> bool {
    act(tool) != Act::Look
}

/// How a call reaches a machine other than the caller's own.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Crossing {
    /// The controller runs it on the far machine as it would for its own.
    Free,
    /// The controller runs it once the person has said yes. `remember` is
    /// whether that yes may stand for the rest of the asking agent's
    /// conversation; a call that destroys state or runs arbitrary code is
    /// approved one run at a time.
    Ask { remember: bool },
    /// Stays on the machine it was called on.
    Never,
}

/// Which way `tool` crosses to another machine.
///
/// Looking is what makes the rest usable. An agent asked to work with another
/// machine has to be able to find out what is on it — which sessions, what is
/// on their screens, which files, what was said — before it can decide who to
/// ask; a list of machine names and a way to send a message into the dark is
/// not a route, it is a rumour. So reading travels freely, and so does saying
/// something to somebody, whose blast radius is a message.
///
/// A change waits on the person: starting a session and typing into one,
/// which a yes may be kept for, and closing one, running a shell, or arming a
/// trigger, which are approved a run at a time. The controller only ever runs
/// what a person approved; it never decides on its own.
///
/// Three kinds of thing never travel. Waiting: a relayed job runs on the
/// controller's own round, so a tool that sits there holds up every other
/// machine's errands, which is why `wait_for` stays home. The fleet's own
/// wiring — enabling a machine, editing SSH configuration — which a machine an
/// agent could not lawfully be on must not become lawful because the agent
/// asked nicely. And the caller's own row, which is only ever its own.
pub fn crossing(tool: &str) -> Crossing {
    match tool {
        "list_directory"
        | "list_files"
        | "list_machines"
        | "list_sessions"
        | "preview_file"
        | "read_conversation"
        | "read_screen"
        | "search_conversations"
        | "search_history"
        | "talk_read"
        | "message_agent"
        | "send_channel_message"
        | "talk_post" => Crossing::Free,
        "launch_session" | "send_input" => Crossing::Ask { remember: true },
        "archive_session" | "delete_session" | "run_shell" | "trigger" => {
            Crossing::Ask { remember: false }
        }
        _ => Crossing::Never,
    }
}

/// Whether the controller carries `tool` to another machine at all.
pub fn travels(tool: &str) -> bool {
    crossing(tool) != Crossing::Never
}

/// Whether `tool`, aimed at another machine, waits on the person.
pub fn needs_approval(tool: &str) -> bool {
    matches!(crossing(tool), Crossing::Ask { .. })
}

/// Whether a yes to `tool` may stand for the rest of the asking agent's
/// conversation, rather than for one run.
pub fn approval_may_stand(tool: &str) -> bool {
    crossing(tool) == Crossing::Ask { remember: true }
}

/// Who begat whom on one machine: every session it knows, with its parent.
pub(crate) type Lineage = Vec<(String, Option<String>)>;

/// The parent links a rule needs, fetched the first time one asks and never
/// twice. Most calls need none: fetching them is a round trip that, across a
/// relay, is a round trip to another machine.
struct Records<F> {
    fetch: Option<F>,
    held: Option<Lineage>,
}

impl<F: FnOnce() -> Result<Lineage>> Records<F> {
    fn get(&mut self) -> Result<&[(String, Option<String>)]> {
        if self.held.is_none()
            && let Some(fetch) = self.fetch.take()
        {
            self.held = Some(fetch()?);
        }
        Ok(self.held.as_deref().unwrap_or(&[]))
    }
}

/// Weigh one call against what the caller holds, before it goes anywhere.
///
/// `lineage` fetches the parent links of the machine the call lands on —
/// this one's, or another machine's over the relay — and is only called when
/// a rule turns on them. The surfaces call this first thing, on every call and
/// on both routes, so a check that passes here is the only check there is.
pub(crate) fn authorize(
    own: &Powers,
    tool: &str,
    arguments: &Value,
    lineage: impl FnOnce() -> Result<Lineage>,
) -> Result<()> {
    let mut records = Records {
        fetch: Some(lineage),
        held: None,
    };
    // Every door into another agent's prompt box, whatever tool it is behind.
    if let Some(target) = written_to(tool, arguments)
        && own.reach != Reach::Fleet
        && !reaches_without_records(own, target)
    {
        check_may_message(own, target, records.get()?)?;
    }
    match act(tool) {
        Act::TellPerson => check_may_reach_person(own),
        Act::Post => check_may_post(own, optional_str(arguments, "scope").unwrap_or("path")),
        Act::Start => {
            // A kind this build cannot read is the tool's own to refuse, in its
            // own words, a moment from now.
            if let Some(kind) =
                optional_str(arguments, "kind").and_then(|kind| kind.parse::<AgentKind>().ok())
            {
                check_may_launch(own, kind)?;
            }
            match optional_str(arguments, "resume_id")
                .map(str::trim)
                .filter(|id| crate::runtime::is_daemon_session_id(id))
            {
                Some(revived) if !own.is_whole() => {
                    check_owns(own, revived, records.get()?, Owning::Resume)
                }
                _ => Ok(()),
            }
        }
        Act::Close => match optional_str(arguments, "session_id").map(str::trim) {
            Some(closed) if !own.is_whole() => {
                check_owns(own, closed, records.get()?, Owning::Close)
            }
            _ => Ok(()),
        },
        Act::Look | Act::Speak | Act::Command | Act::Rewire | Act::NameSelf => Ok(()),
    }
}

/// Every variable a launch writes into a session's environment for the muxloom
/// tools inside it to read: who the session is, where, whose, and what it may
/// do.
///
/// One list, because a runtime that starts its MCP servers with a filtered
/// environment has to be told each of these by name — Codex passes on only a
/// short list of its own unless its server table names more — and a variable
/// left off is a session that reads as nobody's, holding everything.
pub const SESSION_ENVIRONMENT: &[&str] = &[
    "MUXLOOM_SESSION_ID",
    "MUXLOOM_SESSION_KIND",
    "MUXLOOM_SESSION_PATH",
    "MUXLOOM_SESSION_PARENT",
    "MUXLOOM_SESSION_LABEL",
    "MUXLOOM_TASK_ROOT",
    "MUXLOOM_MACHINE",
    "MUXLOOM_MACHINE_LABEL",
    "MUXLOOM_MAY_MESSAGE",
    "MUXLOOM_MAY_LAUNCH",
    "MUXLOOM_MAY_REACH_PERSON",
];

/// What muxloom told this process about the session it is running in.
///
/// `launch_session` puts these in a session's environment, so an agent that
/// lives in one never has to be told — or to guess — who and where it is.
/// Outside a muxloom session they are simply absent, and the board says so
/// rather than inventing an identity.
pub(crate) fn session_env(key: &str) -> Option<String> {
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
pub(crate) fn launching_session() -> Option<String> {
    session_env("MUXLOOM_SESSION_ID")
}

/// Which runtime this session is, when muxloom said. Used to work out what a
/// child of it may be started as when the caller does not say.
fn own_kind() -> Option<AgentKind> {
    session_env("MUXLOOM_SESSION_KIND").and_then(|kind| kind.parse().ok())
}

/// The task the calling session belongs to: what the daemon worked out when it
/// launched the session, and failing that the session itself.
///
/// The fallback is what a daemon too old to have said would have meant anyway —
/// a session nobody started is its own task — so an agent talking to one is
/// alone in its task rather than cut off from the scope.
pub(crate) fn task_root() -> Option<String> {
    session_env("MUXLOOM_TASK_ROOT").or_else(|| session_env("MUXLOOM_SESSION_ID"))
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
pub fn own_powers() -> Powers {
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
pub(crate) fn granted_powers(arguments: &Value, own: &Powers) -> Result<Powers> {
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

/// What a session brought back by a resume holds: what its record held, cut
/// down to what the session bringing it back holds.
///
/// `None` is the record's own grant, unchanged, which is what the daemon does
/// with a revival nobody said anything about — and what a person's agent
/// bringing back its own work should get. Anyone else is capped, so a record
/// that somehow held more than its resumer cannot be the way to more.
pub(crate) fn resumed_powers(own: &Powers, record: Option<&Powers>) -> Option<Powers> {
    if own.is_whole() {
        return None;
    }
    Some(own.narrowed(record.unwrap_or(&Powers::whole())))
}

/// Refuse a launch this session was not given the power to make, in words that
/// say who set the limit and what to do about it. The agent reading this
/// cannot lift it — its parent can, on the next launch — so the answer is
/// where to take the question, not what flag to pass.
pub(crate) fn check_may_launch(own: &Powers, kind: AgentKind) -> Result<()> {
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

/// Which of a session's own a call reaches for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Owning {
    Close,
    Resume,
}

/// Refuse to close or bring back a session that is not this one's own.
///
/// A session another agent started is a pair of hands, and what it holds is
/// the work it was handed: itself, and whatever it started to do that work
/// with. Archiving a sibling, a parent or a stranger is ending work somebody
/// else answers for — and archive and delete both take the whole fleet under
/// the session down with it, so one mistaken call reaches everything that
/// session ever started. Bringing one back is the same question the other way
/// round: it puts a session to work, under its own parent, holding its own
/// grant, and that is not a subagent's to decide for somebody else's.
///
/// A person's own agent is not asked this. It answers to the person, who is the
/// one entitled to say what stops — the tools tell it to ask first, and that
/// is a judgement, not a rule a surface can keep for it.
fn check_owns(
    own: &Powers,
    target: &str,
    lineage: &[(String, Option<String>)],
    owning: Owning,
) -> Result<()> {
    if own.is_whole() {
        return Ok(());
    }
    let Some(mine) = launching_session() else {
        bail!(
            "this process carries a subagent's limits but names no session, so there is no telling \
             which sessions are its own to {}",
            match owning {
                Owning::Close => "close",
                Owning::Resume => "bring back",
            }
        );
    };
    if target == mine || descends_from(lineage, target, std::slice::from_ref(&mine)) {
        return Ok(());
    }
    match owning {
        Owning::Close => bail!(
            "{target} was not started by this session, and a session another agent started \
             closes only itself and what it started itself. Tell whoever owns {target} — or the \
             agent that started you — that it is done with, and let them close it."
        ),
        Owning::Resume => bail!(
            "{target} was not started by this session, and a session another agent started brings \
             back only its own. Ask the agent that started you to resume it, or start a fresh \
             session of your own."
        ),
    }
}

/// How far up a chain of parents one walk goes. A session ten handoffs down
/// is still on the same piece of work, and past that a cycle is the likelier
/// explanation than a team.
///
/// Both surfaces walk with it: the moderator's controller surface answers the
/// same dials as the daemon's for a caller that carries them. Only the list
/// they walk differs — one machine's records, or a target machine's out of
/// the fleet.
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
/// to compare against has already decided what that means, and the callers
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
pub(crate) fn lineage(sessions: &[DaemonSession]) -> Lineage {
    sessions
        .iter()
        .map(|record| (record.id.clone(), record.parent.clone()))
        .collect()
}

/// The same, read back out of a rendered `list_sessions` answer — which is all
/// another machine ever hands over.
#[cfg(unix)]
pub(crate) fn lineage_of_answer(rendered: &str) -> Lineage {
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
pub(crate) fn written_to<'a>(name: &str, arguments: &'a Value) -> Option<&'a str> {
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
///
/// Read off the `scope` word rather than a finished draft, so the answer is the
/// same on the machine that would file the post and on one that is about to
/// hand it to a controller. A word that names no scope is the draft's to
/// refuse, with the list of ones that do.
fn check_may_post(own: &Powers, scope: &str) -> Result<()> {
    let allowed = match scope.trim() {
        "task" => true,
        "path" => own.reach != Reach::Parent,
        "machine" | "global" => own.reach == Reach::Fleet,
        _ => true,
    };
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
pub(crate) fn relayed_caller(arguments: &Value) -> Option<String> {
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
pub(crate) fn stamp_powers(arguments: &mut Value, powers: &Powers) {
    if let (Some(object), Ok(value)) = (arguments.as_object_mut(), serde_json::to_value(powers)) {
        object.insert(POWERS_ARGUMENT.into(), value);
    }
}

/// The grant a relayed launch arrived carrying, if it did.
///
/// Trusted on the same terms as `relayed_caller`: only where there is no
/// environment to read instead. On a machine where an agent could write this
/// itself, its own environment is what answers, and this is never consulted.
pub(crate) fn relayed_powers(arguments: &Value) -> Option<Powers> {
    if session_env("MUXLOOM_SESSION_ID").is_some() {
        return None;
    }
    arguments
        .get(POWERS_ARGUMENT)
        .cloned()
        .and_then(|value| serde_json::from_value(value).ok())
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
pub(crate) fn launch_path_within(arguments: &Value, own: &str) -> Result<String> {
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

/// The environment, rewritten for one test at a time.
///
/// Everything above reads the caller out of process-global variables, so every
/// test that sets them — here, and the surface tests in `control` — waits on
/// this one lock rather than on a lock of its own module's.
#[cfg(test)]
pub(crate) mod test_env {
    pub(crate) fn lock() -> std::sync::MutexGuard<'static, ()> {
        static ENVIRONMENT: std::sync::Mutex<()> = std::sync::Mutex::new(());
        ENVIRONMENT
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Set an environment variable for the duration of a test.
    pub(crate) struct EnvScope {
        key: String,
        previous: Option<String>,
    }

    impl EnvScope {
        pub(crate) fn set(key: &str, value: Option<&str>) -> Self {
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

    /// Clear every variable a launch writes, so a test starts as a process
    /// running in no session whatever shell ran the suite.
    pub(crate) fn nobody() -> Vec<EnvScope> {
        [
            "MUXLOOM_SESSION_ID",
            "MUXLOOM_SESSION_KIND",
            "MUXLOOM_SESSION_PARENT",
            "MUXLOOM_SESSION_PATH",
            "MUXLOOM_TASK_ROOT",
            "MUXLOOM_MAY_MESSAGE",
            "MUXLOOM_MAY_LAUNCH",
            "MUXLOOM_MAY_REACH_PERSON",
        ]
        .into_iter()
        .map(|key| EnvScope::set(key, None))
        .collect()
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{test_env::EnvScope, *};

    fn row(id: &str, parent: Option<&str>) -> (String, Option<String>) {
        (id.to_string(), parent.map(str::to_string))
    }

    /// A subagent with the narrow defaults: its own team, its own runtime, and
    /// the person left to whoever started it.
    fn as_subagent(id: &str, parent: &str) -> Vec<EnvScope> {
        let mut scopes = test_env::nobody();
        scopes.extend([
            EnvScope::set("MUXLOOM_SESSION_ID", Some(id)),
            EnvScope::set("MUXLOOM_SESSION_KIND", Some("claude")),
            EnvScope::set("MUXLOOM_SESSION_PARENT", Some(parent)),
            EnvScope::set("MUXLOOM_TASK_ROOT", Some(parent)),
            EnvScope::set("MUXLOOM_MAY_MESSAGE", Some("task")),
            EnvScope::set("MUXLOOM_MAY_LAUNCH", Some("claude")),
            EnvScope::set("MUXLOOM_MAY_REACH_PERSON", Some("no")),
        ]);
        scopes
    }

    /// Every tool either surface offers is in the table on purpose: a tool
    /// that falls through to the default is a tool nobody decided about.
    #[test]
    fn every_tool_has_an_act_and_a_crossing_somebody_chose() {
        let looks = [
            "list_machines",
            "list_sessions",
            "read_screen",
            "wait_for",
            "talk_read",
            "list_resume_candidates",
            "search_history",
            "search_conversations",
            "read_conversation",
            "list_directory",
            "list_files",
            "search_files",
            "preview_file",
        ];
        for tool in looks {
            assert_eq!(act(tool), Act::Look, "{tool}");
            assert!(!changes_something(tool), "{tool}");
        }
        for tool in [
            "message_agent",
            "send_input",
            "send_channel_message",
            "talk_post",
            "launch_session",
            "archive_session",
            "delete_session",
            "run_shell",
            "trigger",
            "set_machine_enabled",
            "ssh_host",
            "set_head_name",
        ] {
            assert!(changes_something(tool), "{tool} is read-only by mistake");
        }
        // A tool from a newer client is weighed as the widest change, not
        // waved through as a look.
        assert_eq!(act("format_disk"), Act::Command);
        assert!(changes_something("format_disk"));

        // What travels freely is what looks and what speaks; what changes a
        // machine waits on the person; and waiting, the fleet's own wiring,
        // and the caller's own row stay home.
        for tool in looks {
            let crossing = crossing(tool);
            assert!(
                crossing == Crossing::Free || crossing == Crossing::Never,
                "{tool} is a look and waits on nobody"
            );
        }
        assert_eq!(crossing("wait_for"), Crossing::Never);
        for tool in ["message_agent", "send_channel_message", "talk_post"] {
            assert_eq!(crossing(tool), Crossing::Free, "{tool}");
        }
        assert!(approval_may_stand("launch_session"));
        assert!(approval_may_stand("send_input"));
        for tool in ["archive_session", "delete_session", "run_shell", "trigger"] {
            assert!(needs_approval(tool), "{tool}");
            assert!(!approval_may_stand(tool), "{tool} is too sharp to remember");
        }
        for tool in [
            "set_machine_enabled",
            "ssh_host",
            "set_head_name",
            "format_disk",
        ] {
            assert!(!travels(tool), "{tool} must never cross a machine");
        }
    }

    /// The list a filtered runtime is told to pass through has to be the list
    /// a launch writes. A variable the daemon adds and this list does not name
    /// reaches Claude's tools and never Codex's, and nothing else would say so.
    #[test]
    fn the_session_environment_is_every_muxloom_variable_a_launch_writes() {
        let daemon = include_str!("daemon.rs");
        let mut written = Vec::new();
        for (at, _) in daemon.match_indices("keeper_environment.push((") {
            let rest = &daemon[at..];
            let Some(open) = rest.find('"') else { continue };
            let name: String = rest[open + 1..]
                .chars()
                .take_while(|character| *character != '"')
                .collect();
            if name.starts_with("MUXLOOM_") {
                written.push(name);
            }
        }
        written.sort();
        written.dedup();
        assert!(written.len() >= 10, "found only {written:?}");
        for name in &written {
            assert!(
                SESSION_ENVIRONMENT.contains(&name.as_str()),
                "the daemon writes {name} into a session and the list does not pass it on"
            );
        }
        for name in SESSION_ENVIRONMENT {
            assert!(
                written.iter().any(|held| held == name),
                "{name} is never written"
            );
        }
    }

    #[test]
    fn a_relayed_caller_is_taken_only_when_the_environment_says_nothing() {
        let _lock = test_env::lock();
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
        let _lock = test_env::lock();
        let _nobody = test_env::nobody();
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
        let _lock = test_env::lock();
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
        let _lock = test_env::lock();
        let _nobody = test_env::nobody();
        let _id = EnvScope::set("MUXLOOM_SESSION_ID", Some("muxloomd-claude-lead"));
        let _root = EnvScope::set("MUXLOOM_TASK_ROOT", Some("muxloomd-claude-lead"));
        let _reach = EnvScope::set("MUXLOOM_MAY_MESSAGE", Some("task"));
        let _launch = EnvScope::set("MUXLOOM_MAY_LAUNCH", Some("claude"));
        let _person = EnvScope::set("MUXLOOM_MAY_REACH_PERSON", Some("no"));
        let here = vec![
            row("muxloomd-claude-lead", None),
            row("muxloomd-claude-hand", Some("muxloomd-claude-lead")),
            row("muxloomd-claude-grandchild", Some("muxloomd-claude-hand")),
            row("muxloomd-claude-stranger", None),
            row(
                "muxloomd-claude-someone-elses",
                Some("muxloomd-claude-stranger"),
            ),
        ];
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
    #[test]
    fn a_reach_settled_without_records_is_settled_the_same_way_with_them() {
        let _lock = test_env::lock();
        let _subagent = as_subagent("muxloomd-claude-hand", "muxloomd-claude-lead");
        let here = vec![
            row("muxloomd-claude-lead", None),
            row("muxloomd-claude-hand", Some("muxloomd-claude-lead")),
            row("muxloomd-claude-grandchild", Some("muxloomd-claude-hand")),
            row("muxloomd-claude-stranger", None),
        ];
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
        let _lock = test_env::lock();
        let _subagent = as_subagent("muxloomd-claude-hand", "muxloomd-claude-lead");
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
        let _lock = test_env::lock();
        let _subagent = as_subagent("muxloomd-claude-hand", "muxloomd-claude-lead");

        let own = own_powers();
        check_may_post(&own, "task").unwrap();
        check_may_post(&own, "path").unwrap();
        let error = check_may_post(&own, "machine").err().unwrap().to_string();
        assert!(error.contains("scope \"task\" or \"path\""), "{error}");
        assert!(check_may_post(&own, "global").is_err());
        // A word that names no room is the draft's to refuse, with the list.
        check_may_post(&own, "everywhere").unwrap();

        let _reach = EnvScope::set("MUXLOOM_MAY_MESSAGE", Some("parent"));
        let own = own_powers();
        check_may_post(&own, "task").unwrap();
        let error = check_may_post(&own, "path").err().unwrap().to_string();
        assert!(error.contains("its own task's board"), "{error}");

        // Nothing said about this session: a person's agent, and every room
        // is open to it.
        let _reach = EnvScope::set("MUXLOOM_MAY_MESSAGE", None);
        for scope in ["task", "path", "machine", "global"] {
            check_may_post(&own_powers(), scope).unwrap();
        }
    }

    /// The weighing happens before a call is routed, so naming a machine is
    /// not a way past it. A board post handed to a controller used to be
    /// weighed over there, in a process that runs in no session and so holds
    /// everything — and a subagent told to keep to its own task could write to
    /// every machine's board by naming one.
    #[test]
    fn naming_another_machine_does_not_step_past_a_limit() {
        let _lock = test_env::lock();
        let _subagent = as_subagent("muxloomd-claude-hand", "muxloomd-claude-lead");
        let own = own_powers();
        let nothing = || -> Result<Lineage> { Ok(Vec::new()) };

        let post = json!({ "machine": "gpu-1", "scope": "global", "text": "hello" });
        let error = authorize(&own, "talk_post", &post, nothing)
            .err()
            .unwrap()
            .to_string();
        assert!(error.contains("scope \"task\" or \"path\""), "{error}");
        authorize(
            &own,
            "talk_post",
            &json!({ "machine": "gpu-1", "scope": "task", "text": "hi" }),
            nothing,
        )
        .unwrap();

        let tell = json!({ "machine": "gpu-1", "text": "done" });
        assert!(authorize(&own, "send_channel_message", &tell, nothing).is_err());

        // A launch of a runtime it was not handed is refused whichever machine
        // it is aimed at.
        let launch = json!({ "machine": "gpu-1", "kind": "codex", "path": "/w" });
        let error = authorize(&own, "launch_session", &launch, nothing)
            .err()
            .unwrap()
            .to_string();
        assert!(error.contains("codex is not one of them"), "{error}");

        // Looking costs nothing and asks for no records.
        let never = || -> Result<Lineage> { panic!("a look fetched the records") };
        authorize(&own, "read_screen", &json!({ "session_id": "x" }), never).unwrap();
    }

    /// A subagent closes itself and what it started, and nothing else — and
    /// brings back only its own. Archive and delete take a whole fleet down
    /// with them, and a resume puts a session back to work holding its own
    /// grant; neither is a hand's to do to somebody else's work.
    #[test]
    fn a_subagent_closes_and_brings_back_only_its_own() {
        let _lock = test_env::lock();
        let _subagent = as_subagent("muxloomd-claude-hand", "muxloomd-claude-lead");
        let own = own_powers();
        let tree = || -> Result<Lineage> {
            Ok(vec![
                row("muxloomd-claude-lead", None),
                row("muxloomd-claude-hand", Some("muxloomd-claude-lead")),
                row("muxloomd-claude-helper", Some("muxloomd-claude-hand")),
                row("muxloomd-claude-deeper", Some("muxloomd-claude-helper")),
                row("muxloomd-claude-sibling", Some("muxloomd-claude-lead")),
                row("muxloomd-claude-stranger", None),
            ])
        };
        let on = |id: &str| json!({ "session_id": id });
        for mine in [
            "muxloomd-claude-hand",
            "muxloomd-claude-helper",
            "muxloomd-claude-deeper",
        ] {
            authorize(&own, "archive_session", &on(mine), tree).unwrap();
            authorize(&own, "delete_session", &on(mine), tree).unwrap();
        }
        for theirs in [
            "muxloomd-claude-lead",
            "muxloomd-claude-sibling",
            "muxloomd-claude-stranger",
            "muxloomd-claude-nowhere",
        ] {
            let error = authorize(&own, "delete_session", &on(theirs), tree)
                .err()
                .unwrap()
                .to_string();
            assert!(error.contains("closes only itself"), "{error}");
            assert!(error.contains(theirs), "{error}");
        }

        // A resume by muxloom number is the same question; by a runtime's own
        // conversation id it is an ordinary launch, and only the kind counts.
        let resume = |id: &str| json!({ "kind": "claude", "resume_id": id });
        authorize(
            &own,
            "launch_session",
            &resume("muxloomd-claude-helper"),
            tree,
        )
        .unwrap();
        let error = authorize(
            &own,
            "launch_session",
            &resume("muxloomd-claude-stranger"),
            tree,
        )
        .err()
        .unwrap()
        .to_string();
        assert!(error.contains("brings back only its own"), "{error}");
        let never = || -> Result<Lineage> { panic!("a native resume fetched the records") };
        authorize(
            &own,
            "launch_session",
            &resume("0199-native-conversation"),
            never,
        )
        .unwrap();

        // A session that may start nothing may not start something by
        // bringing it back either.
        let _launch = EnvScope::set("MUXLOOM_MAY_LAUNCH", Some(""));
        let error = authorize(
            &own_powers(),
            "launch_session",
            &resume("muxloomd-claude-helper"),
            tree,
        )
        .err()
        .unwrap()
        .to_string();
        assert!(error.contains("may not start others"), "{error}");

        // The person's own agent is not held to it: it answers to the person.
        let _whole = test_env::nobody();
        let whole = own_powers();
        let never = || -> Result<Lineage> { panic!("a person's agent fetched the records") };
        authorize(
            &whole,
            "delete_session",
            &on("muxloomd-claude-stranger"),
            never,
        )
        .unwrap();
    }

    /// A revived session keeps its record's grant when a person's agent brings
    /// it back, and is capped by whoever else does — so dying and coming back
    /// is never the way to more.
    #[test]
    fn a_resume_never_hands_back_more_than_the_resumer_holds() {
        let whole = Powers::whole();
        assert_eq!(resumed_powers(&whole, None), None);
        let narrow = Powers {
            reach: Reach::Task,
            launches: vec![AgentKind::Claude],
            may_reach_person: false,
        };
        assert_eq!(resumed_powers(&narrow, None), Some(narrow.clone()));
        let record = Powers {
            reach: Reach::Parent,
            launches: vec![AgentKind::Claude, AgentKind::Codex],
            may_reach_person: true,
        };
        assert_eq!(
            resumed_powers(&narrow, Some(&record)),
            Some(Powers {
                reach: Reach::Parent,
                launches: vec![AgentKind::Claude],
                may_reach_person: false,
            })
        );
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
}
