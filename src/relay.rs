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
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

use crate::{
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
    "talk_post",
    "talk_read",
];
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

#[derive(Debug)]
struct Entry {
    job: RelayJob,
    /// Whether a controller has picked it up. A job is handed out once.
    taken: bool,
    /// The controller's answer, once it has one.
    answer: Option<RelayAnswer>,
}

/// The daemon's side of the relay: jobs waiting, jobs answered, and when a
/// controller last showed itself.
#[derive(Debug, Default)]
pub struct RelayQueue {
    entries: Vec<Entry>,
    next: u64,
    /// When a controller last asked for work. Zero until one ever has.
    last_poll: u64,
    /// Where the last controller to come round said it could reach, and who it
    /// said this machine was.
    peers: Vec<RelayPeer>,
    /// What the controller calls itself, for saying which way a machine is
    /// reached. Empty until a controller new enough to say has been round.
    via: String,
}

impl RelayQueue {
    /// Whether a controller is watching this machine right now.
    pub fn attached(&self, now: u64) -> bool {
        self.last_poll > 0 && now.saturating_sub(self.last_poll) <= ATTACHED_MS
    }

    /// The machines a controller has said it can reach for this one, and what
    /// to call the way there. Empty when no controller has ever been round, or
    /// when the one that has is too old to say — the caller falls back to
    /// asking the controller directly, which is what it always did.
    pub fn peers(&self) -> (&[RelayPeer], &str) {
        (&self.peers, &self.via)
    }

    /// Queue a job, or say why it cannot be queued. Failing here rather than
    /// waiting is the point: an agent that asks for another machine while no
    /// controller is running should be told so on the call it made.
    pub fn submit(&mut self, tool: &str, arguments: &str, now: u64) -> Result<String> {
        if !relayed(tool) {
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
    /// before there was anything to say. Keep what the last controller that
    /// could talk about it said, rather than forgetting the fleet because an
    /// older one came round.
    pub fn poll(&mut self, now: u64, peers: Vec<RelayPeer>, via: &str) -> Vec<RelayJob> {
        self.last_poll = now;
        if !peers.is_empty() {
            self.peers = peers;
            self.via = via.to_string();
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

/// What one round of errand-running did, for the debug log.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct RelayRound {
    pub ran: usize,
    pub refused: usize,
    pub failed: usize,
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
        let jobs = match pool.relay_poll(target, peers, &via) {
            Ok(jobs) => jobs,
            Err(error) => {
                debug::log("relay", format!("{}: no relay ({error})", target.id));
                continue;
            }
        };
        for job in jobs {
            let (ok, output) = if relayed(&job.tool) {
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
    }
    Ok(round)
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
        let error = queue.submit("list_machines", "{}", t0).unwrap_err();
        assert!(
            error.to_string().contains("attached muxloom controller"),
            "{error}"
        );

        // A controller shows up and the same call goes through.
        assert!(queue.poll(t0, Vec::new(), "").is_empty());
        let id = queue.submit("list_machines", "{}", t0).unwrap();
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
        // Nothing that changes the far machine, and nothing that would sit
        // there: a relayed job runs on the controller's own round.
        for tool in [
            "run_shell",
            "delete_session",
            "archive_session",
            "launch_session",
            "send_input",
            "trigger",
            "ssh_host",
            "set_machine_enabled",
            "wait_for",
        ] {
            let error = queue.submit(tool, "{}", t0).unwrap_err().to_string();
            assert!(error.contains("does not relay"), "{tool}: {error}");
            assert!(error.contains("message_agent"), "{tool}: {error}");
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
            assert!(queue.submit(tool, "{}", t0).is_ok(), "{tool}");
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
        assert!(queue.peers().0.is_empty());

        let fleet = vec![
            RelayPeer {
                id: "local".into(),
                label: "This machine".into(),
                own: false,
            },
            RelayPeer {
                id: "seed".into(),
                label: "seed".into(),
                own: true,
            },
        ];
        queue.poll(t0, fleet.clone(), "laptop");
        let (peers, via) = queue.peers();
        assert_eq!(peers, fleet);
        assert_eq!(via, "laptop");
        // Exactly one of them is this machine, and it is not somewhere to be
        // relayed to.
        assert_eq!(peers.iter().filter(|peer| peer.own).count(), 1);

        // A controller from before there was anything to say leaves the fleet
        // alone rather than emptying it: it cannot reach nothing at all, so an
        // empty list is silence, not news.
        queue.poll(t0 + 1, Vec::new(), "");
        assert_eq!(queue.peers().0, fleet);
        assert_eq!(queue.peers().1, "laptop");
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
        let id = queue.submit("read_conversation", "{}", t0).unwrap();
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
