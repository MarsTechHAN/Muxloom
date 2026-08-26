//! Borrowing the controller's reach.
//!
//! An agent that runs `muxloomd mcp` can only see the machine it is on: a
//! daemon has one socket, one set of sessions, and no idea that any other
//! machine exists. The controller is the only thing that sees all of them, so
//! anything cross-machine has to be run by it. This is that errand service.
//!
//! The daemon holds a small queue. An agent submits a job, whatever controller
//! is attached takes it on its next round, runs it against its own tool
//! surface, and writes the answer back; the agent polls until it appears. The
//! controller decides what it is willing to run — the allowlist here covers
//! looking and speaking, and the things left off it are exactly the ones that
//! would let an agent on one machine reach into another's shell, SSH
//! configuration, or session list with a delete. A machine's own daemon is
//! still the only thing that can do those, to its own machine, for whoever is
//! standing on it.
//!
//! The same round is how a daemon finds out the other machines are there at
//! all. It never opens a connection and never goes looking: a controller names
//! the machines it can reach, and those are the daemon's neighbours for as long
//! as that controller is running. So two machines with no route between them
//! are neighbours through the one that can see both — which is the whole of the
//! mesh, and why nothing here has to be discovered.
//!
//! Nothing here waits: a submit fails at once if no controller has asked for
//! work recently, because the honest answer to "can you reach that machine" is
//! either yes now or no.

use std::{
    fmt,
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

use crate::{
    approvals::{Approvals, Pending as ApprovalsPending},
    config::Config,
    control::{ControlSurface, ControllerControl},
    debug,
    model::Target,
    runtime::Runtime,
};

/// Capability that says a daemon will pass work to an attached controller.
pub const RELAY_CAPABILITY: &str = "relay-v1";
/// The tools a controller runs on another machine's behalf: everything that
/// looks, and everything that says something to somebody. Nothing that changes
/// a machine, and nothing whose blast radius is larger than a message.
///
/// Looking is the half that makes the rest usable. An agent asked to work with
/// another machine has to be able to find out what is on it — which sessions,
/// what is on their screens, which files, what was said — before it can decide
/// who to ask; a list of machine names and a way to send a message into the
/// dark is not a route, it is a rumour. So reading is relayed freely, and the
/// line stays exactly where it was: the tools left off this list are the ones
/// that would let an agent on one machine run a command, start or end a
/// session, type into one, or edit how the fleet is wired. Those belong to the
/// machine's own daemon, for whoever is standing on it.
///
/// Waiting is the one thing that does not travel: a relayed job runs on the
/// controller's own round, so a tool that sits there holds up every other
/// machine's errands and the talk board with them. `wait_for` is therefore not
/// here, and the tools that can be told to wait are run without the waiting.
pub const RELAYED_TOOLS: &[&str] = &[
    "list_directory",
    "list_files",
    "list_machines",
    "list_sessions",
    "message_agent",
    "preview_file",
    "read_conversation",
    "read_screen",
    "search_conversations",
    "search_history",
    // Speaking to the human, for a machine that has not been handed the
    // credentials yet. It changes nothing over there — the controller says
    // something on this machine's behalf, exactly as `message_agent` does.
    "send_channel_message",
    "talk_post",
    "talk_read",
];

/// Cross-machine WRITE tools a daemon-flavoured agent may run on another
/// machine once a person has approved it. These are the ones that act on a
/// session a human would expect to sign off on: starting one, and typing into
/// one. `relay.rs` lets the controller execute them, but only after the
/// approval gate (see `control.rs`) lets them through; the controller itself
/// never prompts, it only runs what an approved daemon submitted.
pub const APPROVE_TOOLS: &[&str] = &["launch_session", "send_input"];

/// Cross-machine WRITE tools that are sensitive enough that a one-shot yes is
/// all a person should give: they destroy state or run arbitrary code. They
/// may be approved once, but never remembered for the rest of the
/// conversation.
pub const SENSITIVE_TOOLS: &[&str] = &["delete_session", "archive_session", "run_shell", "trigger"];

/// Whether `tool` is held behind the cross-machine approval gate.
pub fn approve_gated(tool: &str) -> bool {
    APPROVE_TOOLS.contains(&tool) || SENSITIVE_TOOLS.contains(&tool)
}

/// Whether an approved cross-machine WRITE may be remembered for the rest of
/// the conversation (false = one-shot only).
pub fn reminder_allowed(tool: &str) -> bool {
    APPROVE_TOOLS.contains(&tool)
}
/// How long a controller's last request for work counts for. A controller
/// carries the board every couple of seconds and asks for work on the same
/// round, so a gap this size means it is gone, not busy.
pub const ATTACHED_MS: u64 = 20_000;
/// How long a job waits to be taken and answered before it is dropped.
pub const EXPIRY_MS: u64 = 120_000;
/// How many jobs may be outstanding at once.
const QUEUE_LIMIT: usize = 32;

/// One piece of work waiting for a controller.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RelayJob {
    pub id: String,
    /// The tool to run, which the controller checks against the allowlist
    /// again before running it.
    pub tool: String,
    /// Its arguments as JSON text. The wire type stays a string so the
    /// protocol's messages keep comparing by value.
    pub arguments: String,
    /// The session that asked, when one did. Carried so a person's "always for
    /// this conversation" can be scoped to the asking agent rather than to
    /// every agent on the machine. Empty when the ask came from the surface
    /// directly with no session in context.
    #[serde(default)]
    pub session: String,
    pub submitted_at: u64,
}

/// A machine some controller can reach, told to a daemon that cannot reach it
/// itself.
///
/// This is the whole of the mesh, and it is deliberately not discovery. A
/// daemon never opens an SSH connection and never guesses at a neighbour: it
/// knows exactly the machines a controller has come round and named, which are
/// the machines that user enabled. Two machines that cannot see each other are
/// neighbours only for as long as a controller that sees both is running.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct RelayPeer {
    /// How the controller addresses it, which is how a tool call must.
    pub id: String,
    #[serde(default)]
    pub label: String,
    /// The machine this daemon is itself on, named as the controller names it.
    /// At most one peer in a poll carries this, and it is the one place a call
    /// does not have to be relayed to.
    #[serde(default)]
    pub own: bool,
    /// What the controller offering this reach calls itself. Filled in on the
    /// way out of a daemon, not on the way in: on the way in the poll says who
    /// is asking, and every peer in it is theirs.
    #[serde(default)]
    pub via: String,
}

/// What came back, or that nothing has yet.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct RelayAnswer {
    /// Whether the controller has answered at all.
    pub done: bool,
    /// Whether the tool succeeded. Meaningless until `done`.
    #[serde(default)]
    pub ok: bool,
    /// The tool's output, or its error.
    #[serde(default)]
    pub output: String,
}

/// Whether the controller will run this tool for another machine.
pub fn relayed(tool: &str) -> bool {
    RELAYED_TOOLS.contains(&tool)
}

/// The refusal an off-list tool gets, worded the same wherever it is caught:
/// the daemon refuses on submit, and the controller refuses again on pickup.
pub fn refusal(tool: &str) -> String {
    format!(
        "muxloom does not relay {tool} to another machine. A controller will look at another \
         machine for you and say things on it, but changing one — a command, a session started, \
         ended or typed into, how the fleet is wired — belongs to that machine's own agents. Ask \
         one with message_agent."
    )
}

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u128::from(u64::MAX)) as u64
}

/// Source of fresh approval ids while the controller runs. A plain counter is
/// all the uniqueness a chat reply needs.
static NEXT_PENDING: AtomicU64 = AtomicU64::new(0);

/// Which machine a relayed job names, if it names one. Read from the
/// `machine` argument the same way the tool surface does, so the ask tells the
/// person where the write would land and the remember key is stable.
fn job_machine(job: &RelayJob) -> String {
    serde_json::from_str::<serde_json::Value>(&job.arguments)
        .ok()
        .and_then(|value| {
            value
                .get("machine")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string)
        })
        .unwrap_or_default()
}

/// Ask the person over the bound chat, when a chat is bound and reachable.
/// Not reachable is fine: the job is refused with the same "needs approval"
/// answer, and the refusal itself tells the agent what to type back.
fn try_chat_ask(surface: &mut Option<ControllerControl>, ask: &str) {
    let Some(surface) = surface else {
        return;
    };
    let _ = surface.call(
        "send_channel_message",
        &serde_json::json!({ "text": ask, "title": "approval needed" }),
    );
}

#[derive(Debug)]
struct Entry {
    job: RelayJob,
    /// Whether a controller has picked it up. A job is handed out once.
    taken: bool,
    /// The controller's answer, once it has one.
    answer: Option<RelayAnswer>,
}

/// What one controller says it can reach, and when it last said so. Kept per
/// controller rather than as one list: two dashboards may watch the same
/// machine, and one of them naming its fleet must not erase the other's.
#[derive(Debug)]
struct Reach {
    /// What that controller calls itself.
    via: String,
    peers: Vec<RelayPeer>,
    at: u64,
}

/// The daemon's side of the relay: jobs waiting, jobs answered, and when a
/// controller last showed itself.
#[derive(Debug, Default)]
pub struct RelayQueue {
    entries: Vec<Entry>,
    next: u64,
    /// When a controller last asked for work. Zero until one ever has.
    last_poll: u64,
    /// Where each controller that has been round said it could reach.
    reaches: Vec<Reach>,
}

impl RelayQueue {
    /// Whether a controller is watching this machine right now.
    pub fn attached(&self, now: u64) -> bool {
        self.last_poll > 0 && now.saturating_sub(self.last_poll) <= ATTACHED_MS
    }

    /// Every machine a controller here right now says it can reach, each
    /// stamped with the way there. A machine two controllers both offer is
    /// listed once; whichever spoke last carries it, and either is a route.
    ///
    /// Empty when no controller has been round lately, or when the ones that
    /// have are too old to say — the caller falls back to asking a controller
    /// directly, which is what it always did.
    pub fn peers(&self, now: u64) -> Vec<RelayPeer> {
        let mut merged: Vec<RelayPeer> = Vec::new();
        for reach in self.live(now) {
            for peer in &reach.peers {
                let peer = RelayPeer {
                    via: reach.via.clone(),
                    ..peer.clone()
                };
                match merged.iter_mut().find(|seen| seen.id == peer.id) {
                    // `own` is the daemon's own machine, and one controller
                    // knowing which that is settles it for all of them.
                    Some(seen) => {
                        *seen = RelayPeer {
                            own: seen.own || peer.own,
                            ..peer
                        }
                    }
                    None => merged.push(peer),
                }
            }
        }
        merged
    }

    fn live(&self, now: u64) -> impl Iterator<Item = &Reach> {
        self.reaches
            .iter()
            .filter(move |reach| now.saturating_sub(reach.at) <= ATTACHED_MS)
    }

    /// Queue a job, or say why it cannot be queued. Failing here rather than
    /// waiting is the point: an agent that asks for another machine while no
    /// controller is running should be told so on the call it made.
    pub fn submit(
        &mut self,
        tool: &str,
        arguments: &str,
        session: &str,
        now: u64,
    ) -> Result<String> {
        // A cross-machine WRITE tool is queued too, but it is held behind the
        // person's approval when the controller runs it. It is not refused at
        // the door: the agent should be told it is waiting on a person, not
        // that the machine cannot be reached at all.
        if !relayed(tool) && !approve_gated(tool) {
            bail!("{}", refusal(tool));
        }
        if !self.attached(now) {
            bail!(
                "cross-machine features need an attached muxloom controller, and nothing is \
                 asking this machine for work. Everything on this machine still works; ask an \
                 agent over there instead, or tell whoever runs muxloom that it is not running."
            );
        }
        self.expire(now);
        if self.entries.len() >= QUEUE_LIMIT {
            bail!("too many relayed requests are already waiting on the controller; try again");
        }
        self.next += 1;
        let id = format!("relay-{}", self.next);
        self.entries.push(Entry {
            job: RelayJob {
                id: id.clone(),
                tool: tool.into(),
                arguments: arguments.into(),
                session: session.to_string(),
                submitted_at: now,
            },
            taken: false,
            answer: None,
        });
        Ok(id)
    }

    /// A controller asking for work, and saying where it can reach while it is
    /// here. Answering also records that a controller exists at all, which is
    /// what [`attached`](Self::attached) reads.
    ///
    /// An empty peer list is not the same as no machines: a controller can
    /// always reach the one it is polling, so nothing to say means a build from
    /// before there was anything to say. Such a poll leaves the fleet alone
    /// rather than emptying it.
    pub fn poll(&mut self, now: u64, peers: Vec<RelayPeer>, via: &str) -> Vec<RelayJob> {
        self.last_poll = now;
        if !peers.is_empty() {
            match self.reaches.iter_mut().find(|reach| reach.via == via) {
                Some(reach) => {
                    reach.peers = peers;
                    reach.at = now;
                }
                None => self.reaches.push(Reach {
                    via: via.to_string(),
                    peers,
                    at: now,
                }),
            }
            // A controller that stopped coming round is not a route any more,
            // and its entry would otherwise sit here for the life of the
            // daemon offering machines nothing can carry a call to.
            self.reaches
                .retain(|reach| now.saturating_sub(reach.at) <= EXPIRY_MS);
        }
        self.expire(now);
        self.entries
            .iter_mut()
            .filter(|entry| !entry.taken && entry.answer.is_none())
            .map(|entry| {
                entry.taken = true;
                entry.job.clone()
            })
            .collect()
    }

    /// A controller writing back what happened. An answer for a job that has
    /// already expired is dropped: nobody is waiting for it.
    pub fn complete(&mut self, id: &str, ok: bool, output: String, now: u64) {
        self.last_poll = now;
        if let Some(entry) = self.entries.iter_mut().find(|entry| entry.job.id == id) {
            entry.answer = Some(RelayAnswer {
                done: true,
                ok,
                output,
            });
        }
    }

    /// What the agent that submitted the job sees. An answered job is taken
    /// off the queue as it is read — one job, one submitter, one answer.
    pub fn result(&mut self, id: &str, now: u64) -> Result<RelayAnswer> {
        self.expire(now);
        let Some(position) = self.entries.iter().position(|entry| entry.job.id == id) else {
            bail!(
                "relayed request {id} is gone: the controller did not answer within {} seconds",
                EXPIRY_MS / 1000
            );
        };
        if self.entries[position].answer.is_none() {
            return Ok(RelayAnswer::default());
        }
        Ok(self.entries.remove(position).answer.unwrap_or_default())
    }

    /// Drop jobs nobody can still be waiting for. A controller that died
    /// holding a job leaves it taken and unanswered, so age is the only thing
    /// that can clear it.
    fn expire(&mut self, now: u64) {
        self.entries
            .retain(|entry| now.saturating_sub(entry.job.submitted_at) < EXPIRY_MS);
    }
}

/// What one round of errand-running did, for the debug log — and what it heard
/// about while it was there.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct RelayRound {
    pub ran: usize,
    pub refused: usize,
    pub failed: usize,
    /// Machines some other controller told these daemons it could reach, and
    /// this one cannot. A dashboard shows them so the fleet does not look
    /// smaller than it is; it cannot open a session on one, because the way
    /// there belongs to the controller named on it.
    pub heard: Vec<RelayPeer>,
}

impl RelayRound {
    /// Whether any errand actually happened, which is what the debug log is
    /// about. Hearing about a machine is not work done.
    pub fn busy(&self) -> bool {
        self.ran + self.refused + self.failed > 0
    }
}

impl fmt::Display for RelayRound {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "relayed {} job(s), {} refused, {} failed",
            self.ran, self.refused, self.failed
        )
    }
}

/// Run one round of errands for every machine, this one included: an agent on
/// the controller's own machine talks to its daemon too, and is just as blind
/// to the other machines as one across the network.
///
/// The tool surface is built once per round, and only when a job is actually
/// waiting: it reads the state file, which decides which machines are
/// reachable, and that answer must be this round's rather than this session's.
pub fn run_pump(runtime: &Runtime, config: &Config, targets: &[Target]) -> Result<RelayRound> {
    let pool = runtime.bridge_pool();
    let mut round = RelayRound::default();
    let mut surface: Option<ControllerControl> = None;
    let local = Target::local();
    let everywhere: Vec<&Target> = std::iter::once(&local)
        .chain(targets.iter().filter(|it| it.id != local.id))
        .collect();
    let via = crate::talk::hostname();
    // What this controller can reach, which is the same list for everyone: the
    // machines the user enabled. Each daemon is told which of them is itself,
    // so it knows the one place a call does not have to come back through here.
    //
    // `local` and "This machine" are names that only mean something from inside
    // this controller; every daemon already calls its own machine `local`. Out
    // in the fleet this machine is a machine like the others, and the name it
    // answers to is its hostname — the same word as the way to everywhere else,
    // because it is the same machine. `ControllerControl` reads that name back.
    let reach: Vec<RelayPeer> = everywhere
        .iter()
        .map(|target| {
            let here = target.id == local.id;
            RelayPeer {
                id: if here { via.clone() } else { target.id.clone() },
                label: if here {
                    via.clone()
                } else {
                    target.label.clone()
                },
                own: false,
                // The asking controller names itself in the poll, and every
                // peer in one is its own; a daemon stamps them on the way out.
                via: String::new(),
            }
        })
        .collect();
    for (mine, target) in everywhere.iter().enumerate() {
        let peers = reach
            .iter()
            .enumerate()
            .map(|(at, peer)| RelayPeer {
                own: at == mine,
                ..peer.clone()
            })
            .collect();
        let (jobs, known) = match pool.relay_poll(target, peers, &via) {
            Ok(answer) => answer,
            Err(error) => {
                debug::log("relay", format!("{}: no relay ({error})", target.id));
                continue;
            }
        };
        hear(&mut round.heard, known, &reach, &via);
        let mut approvals = Approvals::load(&Approvals::default_path());
        let mut approval_dirty = false;
        let mut next = NEXT_PENDING.fetch_add(1_000, Ordering::Relaxed) + 1;
        for job in jobs {
            // A WRITE tool held behind the approval gate runs only after the
            // person has said so — remembered for the session, or asked now.
            if approve_gated(&job.tool)
                && !approvals.remembered(&job.session, &job_machine(&job), &job.tool)
            {
                let id = format!("approve-{next}");
                next += 1;
                let machine = job_machine(&job);
                let ask = format!(
                    "An agent wants to run `{}`{} on {}...
Reply `approve-{id}` to allow once, `always-{id}` for the whole conversation, or `reject-{id}` to deny.",
                    job.tool,
                    if machine.is_empty() {
                        ""
                    } else {
                        " (cross-machine)"
                    },
                    if machine.is_empty() {
                        "a remote machine".to_string()
                    } else {
                        machine.clone()
                    }
                );
                approvals.park(
                    id.clone(),
                    ApprovalsPending {
                        session: job.session.clone(),
                        machine: machine.clone(),
                        tool: job.tool.clone(),
                        ask: ask.clone(),
                        at_ms: now_ms(),
                        open: true,
                    },
                );
                approval_dirty = true;
                let (ok, output) = (
                    false,
                    format!(
                        "{tool} needs human approval — the person has been asked via chat; reply `approve-{id}` / `always-{id}` / `reject-{id}`",
                        tool = job.tool
                    ),
                );
                let _ = pool.relay_complete(target, job.id.clone(), ok, output);
                try_chat_ask(&mut surface, &ask);
                continue;
            }
            let (ok, output) = if relayed(&job.tool) || approve_gated(&job.tool) {
                let surface = match &mut surface {
                    Some(surface) => surface,
                    none => none.insert(ControllerControl::with_runtime(
                        config.clone(),
                        runtime.clone(),
                    )?),
                };
                match run(surface, &job.tool, &job.arguments) {
                    Ok(output) => {
                        round.ran += 1;
                        // A one-shot grant is spent by the run it let through.
                        if approve_gated(&job.tool)
                            && approvals.once.iter().any(|(s, m, t)| {
                                s == &job.session && m == &job_machine(&job) && t == &job.tool
                            })
                        {
                            approvals.spend_once(&job.session, &job_machine(&job), &job.tool);
                            approval_dirty = true;
                        }
                        (true, output)
                    }
                    Err(error) => {
                        round.failed += 1;
                        (false, format!("{error:#}"))
                    }
                }
            } else {
                // The daemon refuses these on submit; a job that arrives here
                // anyway came from something other than the muxloom adapter,
                // and the answer is the same either way.
                round.refused += 1;
                (false, refusal(&job.tool))
            };
            if let Err(error) = pool.relay_complete(target, job.id.clone(), ok, output) {
                debug::log(
                    "relay",
                    format!("{}: {} went unanswered ({error})", target.id, job.id),
                );
            }
        }
        if approval_dirty {
            let path = Approvals::default_path();
            if let Err(error) = approvals.save(&path) {
                debug::log("approval", format!("could not save: {error:#}"));
            }
        }
    }
    Ok(round)
}

/// What a daemon's answer adds to the list of machines this controller cannot
/// reach itself. Its own reach is not news, the machine it is talking to is
/// not news, and a machine two daemons both know about is one machine.
fn hear(heard: &mut Vec<RelayPeer>, known: Vec<RelayPeer>, mine: &[RelayPeer], via: &str) {
    for peer in known {
        if peer.via == via
            || peer.own
            || mine.iter().any(|carried| carried.id == peer.id)
            || heard.iter().any(|seen| seen.id == peer.id)
        {
            continue;
        }
        heard.push(peer);
    }
}

/// Run one job against the controller's own tools. The arguments crossed the
/// wire as text, so a malformed set is the submitter's error to hear.
fn run(surface: &mut ControllerControl, tool: &str, arguments: &str) -> Result<String> {
    let arguments = serde_json::from_str(arguments)
        .unwrap_or_else(|_| serde_json::Value::Object(Default::default()));
    surface.call(tool, &bounded(arguments))
}

/// The arguments a relayed job actually runs with.
///
/// One controller runs every machine's errands on the same round as the talk
/// board, so a job that sits and waits stops all of them. Waiting is what gets
/// dropped: a read comes back with what is on the board now, and a message is
/// delivered on the bounded schedule rather than held until its session goes
/// quiet. The asker loses patience it can spend itself — it can look again —
/// and keeps an answer arriving inside the minute.
fn bounded(mut arguments: serde_json::Value) -> serde_json::Value {
    if let Some(map) = arguments.as_object_mut() {
        map.remove("wait_seconds");
        if map.get("deliver").and_then(serde_json::Value::as_str) == Some("when_idle") {
            map.insert("deliver".into(), "auto".into());
        }
    }
    arguments
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_job_only_travels_while_a_controller_is_asking_for_work() {
        let mut queue = RelayQueue::default();
        let t0 = 1_000_000;

        // Nothing has ever polled: the agent is told now, not in a minute.
        let error = queue.submit("list_machines", "{}", "", t0).unwrap_err();
        assert!(
            error.to_string().contains("attached muxloom controller"),
            "{error}"
        );

        // A controller shows up and the same call goes through.
        assert!(queue.poll(t0, Vec::new(), "").is_empty());
        let id = queue.submit("list_machines", "{}", "", t0).unwrap();
        assert!(!id.is_empty(), "a relayed job gets an id");
        // Still nothing back, and asking does not consume the job.
        assert_eq!(queue.result(&id, t0).unwrap(), RelayAnswer::default());

        // It is handed out once, however often the controller asks.
        let jobs = queue.poll(t0 + 100, Vec::new(), "");
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].tool, "list_machines");
        assert!(queue.poll(t0 + 200, Vec::new(), "").is_empty());

        queue.complete(&id, true, "[]".into(), t0 + 300);
        let answer = queue.result(&id, t0 + 400).unwrap();
        assert!(answer.done && answer.ok);
        assert_eq!(answer.output, "[]");
        // Read once: a second look finds nothing, and says so.
        assert!(queue.result(&id, t0 + 500).is_err());
    }

    #[test]
    fn the_controller_runs_errands_not_commands() {
        let mut queue = RelayQueue::default();
        let t0 = 2_000_000;
        queue.poll(t0, Vec::new(), "");
        // Nothing that would sit there: a relayed job runs on the controller's
        // own round, so a wait is refused wherever it is named.
        let error = queue
            .submit("wait_for", "{}", "", t0)
            .unwrap_err()
            .to_string();
        assert!(error.contains("does not relay"), "wait_for: {error}");
        // A cross-machine WRITE is queued (it is held behind the controller's
        // approval gate when it runs, not refused at the door): an agent that
        // asks for another machine is told it is waiting on a person, not that
        // the machine cannot be reached. The two that can never be lawfully
        // crossed — enabling a machine and editing SSH — stay refused.
        for tool in [
            "run_shell",
            "delete_session",
            "archive_session",
            "launch_session",
            "send_input",
            "trigger",
        ] {
            assert!(
                queue.submit(tool, "{}", "", t0).is_ok(),
                "{tool} is gated, not refused"
            );
        }
        for tool in ["ssh_host", "set_machine_enabled"] {
            let error = queue.submit(tool, "{}", "", t0).unwrap_err().to_string();
            assert!(error.contains("does not relay"), "{tool}: {error}");
        }
        // Looking is the half that makes asking possible, so all of it travels.
        for tool in [
            "search_conversations",
            "read_screen",
            "list_files",
            "list_directory",
            "preview_file",
            "search_history",
            "talk_read",
            "talk_post",
        ] {
            assert!(queue.submit(tool, "{}", "", t0).is_ok(), "{tool}");
        }
    }

    #[test]
    fn a_relayed_call_never_sits_on_the_controllers_round() {
        // Waiting is what a controller cannot afford to do on behalf of one
        // machine: it carries the board and everyone else's errands too.
        let read = bounded(serde_json::json!({
            "scope": "direct",
            "wait_seconds": 120,
        }));
        assert_eq!(read, serde_json::json!({ "scope": "direct" }));
        // A delivery that would hold until a session goes quiet falls back to
        // the bounded wait rather than being refused: the message still lands.
        let message = bounded(serde_json::json!({
            "text": "ready when you are",
            "deliver": "when_idle",
        }));
        assert_eq!(message["deliver"], "auto");
        assert_eq!(message["text"], "ready when you are");
        // Anything else is the caller's to say and travels untouched.
        let asked = serde_json::json!({ "deliver": "now", "session_id": "s" });
        assert_eq!(bounded(asked.clone()), asked);
    }

    #[test]
    fn a_daemon_knows_only_the_fleet_a_controller_came_round_and_named() {
        let mut queue = RelayQueue::default();
        let t0 = 4_000_000;

        // Nothing has been round: this machine knows of no other, and the
        // caller is left to ask the controller the old way.
        assert!(queue.peers(t0).is_empty());

        let fleet = vec![
            RelayPeer {
                id: "laptop".into(),
                label: "laptop".into(),
                ..Default::default()
            },
            RelayPeer {
                id: "seed".into(),
                label: "seed".into(),
                own: true,
                ..Default::default()
            },
        ];
        queue.poll(t0, fleet.clone(), "laptop");
        let peers = queue.peers(t0);
        assert_eq!(
            peers
                .iter()
                .map(|peer| peer.id.as_str())
                .collect::<Vec<_>>(),
            ["laptop", "seed"]
        );
        // Each one carries the way to it, which is the controller that named
        // it — the daemon stamps that on, since the peers arrived unsigned.
        assert!(peers.iter().all(|peer| peer.via == "laptop"));
        // Exactly one of them is this machine, and it is not somewhere to be
        // relayed to.
        assert_eq!(peers.iter().filter(|peer| peer.own).count(), 1);

        // A controller from before there was anything to say leaves the fleet
        // alone rather than emptying it: it cannot reach nothing at all, so an
        // empty list is silence, not news.
        queue.poll(t0 + 1, Vec::new(), "");
        assert_eq!(queue.peers(t0 + 1).len(), 2);

        // A second dashboard watching the same machine adds what only it can
        // reach; it does not replace what the first one offers.
        queue.poll(
            t0 + 2,
            vec![
                RelayPeer {
                    id: "seed".into(),
                    label: "seed".into(),
                    own: true,
                    ..Default::default()
                },
                RelayPeer {
                    id: "gpu".into(),
                    label: "gpu".into(),
                    ..Default::default()
                },
            ],
            "desk",
        );
        let peers = queue.peers(t0 + 2);
        let named = |id: &str| {
            peers
                .iter()
                .find(|peer| peer.id == id)
                .unwrap_or_else(|| panic!("{id} missing"))
                .clone()
        };
        assert_eq!(peers.len(), 3);
        assert_eq!(named("laptop").via, "laptop");
        assert_eq!(named("gpu").via, "desk");
        assert!(named("seed").own);

        // A controller that stopped coming round stops being a way anywhere.
        let later = t0 + ATTACHED_MS + 1;
        queue.poll(
            later,
            vec![RelayPeer {
                id: "gpu".into(),
                label: "gpu".into(),
                ..Default::default()
            }],
            "desk",
        );
        assert_eq!(
            queue
                .peers(later)
                .iter()
                .map(|peer| peer.id.clone())
                .collect::<Vec<_>>(),
            ["gpu"]
        );
    }

    #[test]
    fn a_controller_hears_about_the_machines_it_cannot_reach_and_only_those() {
        let peer = |id: &str, via: &str, own: bool| RelayPeer {
            id: id.into(),
            label: id.into(),
            own,
            via: via.into(),
        };
        // What this controller carries: itself and one machine it can ssh to.
        let mine = vec![peer("laptop", "", false), peer("seed", "", false)];
        let mut heard = Vec::new();

        hear(
            &mut heard,
            vec![
                // Its own reach, read back off the daemon it just told.
                peer("laptop", "laptop", false),
                peer("seed", "laptop", true),
                // Another dashboard's, which is the only news here.
                peer("seed", "desk", true),
                peer("gpu", "desk", false),
                peer("desk", "desk", false),
            ],
            &mine,
            "laptop",
        );
        assert_eq!(
            heard
                .iter()
                .map(|peer| peer.id.as_str())
                .collect::<Vec<_>>(),
            ["gpu", "desk"]
        );
        assert_eq!(heard[0].via, "desk");

        // A second daemon that hears about the same machine adds nothing.
        hear(
            &mut heard,
            vec![peer("gpu", "desk", false)],
            &mine,
            "laptop",
        );
        assert_eq!(heard.len(), 2);
    }

    #[test]
    fn a_poll_from_before_the_fleet_was_ever_mentioned_still_parses() {
        use crate::daemon_protocol::DaemonRequest;
        // The field is an addition, so the bare ask a 0.5.5 controller sends
        // has to keep meaning what it meant.
        let bare: DaemonRequest = serde_json::from_str(r#"{"method":"relay_poll"}"#).unwrap();
        assert_eq!(
            bare,
            DaemonRequest::RelayPoll {
                peers: Vec::new(),
                via: String::new(),
            }
        );
    }

    #[test]
    fn a_controller_that_walks_off_mid_errand_does_not_strand_the_asker() {
        let mut queue = RelayQueue::default();
        let t0 = 3_000_000;
        queue.poll(t0, Vec::new(), "");
        let id = queue.submit("read_conversation", "{}", "", t0).unwrap();
        queue.poll(t0, Vec::new(), "");
        // The controller took it and never came back.
        assert!(!queue.result(&id, t0 + EXPIRY_MS - 1).unwrap().done);
        let error = queue.result(&id, t0 + EXPIRY_MS).unwrap_err();
        assert!(error.to_string().contains("did not answer"), "{error}");
        // And a controller that has not asked for work in a while is gone.
        assert!(queue.attached(t0 + ATTACHED_MS));
        assert!(!queue.attached(t0 + ATTACHED_MS + 1));
    }
}
