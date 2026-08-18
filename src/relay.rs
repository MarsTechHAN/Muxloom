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
//! controller decides what it is willing to run — the allowlist here is short
//! on purpose, and the things left off it are exactly the ones that would let
//! an agent on one machine reach into another's shell, SSH configuration, or
//! session list with a delete. A machine's own daemon is still the only thing
//! that can do those, to its own machine, for whoever is standing on it.
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
/// The only tools a controller runs on another machine's behalf. Reading what
/// exists and saying things to people: nothing that changes a machine, and
/// nothing whose blast radius is larger than a message.
pub const RELAYED_TOOLS: &[&str] = &[
    "list_machines",
    "list_sessions",
    "message_agent",
    "read_conversation",
    "search_conversations",
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
        "muxloom does not relay {tool} to another machine. Only {} can be run on your behalf; \
         anything else has to be done by an agent on that machine, which you can ask with \
         message_agent.",
        RELAYED_TOOLS.join(", ")
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
}

impl RelayQueue {
    /// Whether a controller is watching this machine right now.
    pub fn attached(&self, now: u64) -> bool {
        self.last_poll > 0 && now.saturating_sub(self.last_poll) <= ATTACHED_MS
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

    /// A controller asking for work. Handing out a job also records that a
    /// controller is here, which is what [`attached`](Self::attached) reads.
    pub fn poll(&mut self, now: u64) -> Vec<RelayJob> {
        self.last_poll = now;
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
    let everywhere = std::iter::once(&local).chain(targets.iter().filter(|it| it.id != local.id));
    for target in everywhere {
        let jobs = match pool.relay_poll(target) {
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
    surface.call(tool, &arguments)
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
        assert!(queue.poll(t0).is_empty());
        let id = queue.submit("list_machines", "{}", t0).unwrap();
        // Still nothing back, and asking does not consume the job.
        assert_eq!(queue.result(&id, t0).unwrap(), RelayAnswer::default());

        // It is handed out once, however often the controller asks.
        let jobs = queue.poll(t0 + 100);
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].tool, "list_machines");
        assert!(queue.poll(t0 + 200).is_empty());

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
        queue.poll(t0);
        for tool in [
            "run_shell",
            "delete_session",
            "archive_session",
            "launch_session",
            "send_input",
            "trigger",
            "ssh_host",
            "set_machine_enabled",
        ] {
            let error = queue.submit(tool, "{}", t0).unwrap_err().to_string();
            assert!(error.contains("does not relay"), "{tool}: {error}");
            assert!(error.contains("message_agent"), "{tool}: {error}");
        }
        assert!(queue.submit("search_conversations", "{}", t0).is_ok());
    }

    #[test]
    fn a_controller_that_walks_off_mid_errand_does_not_strand_the_asker() {
        let mut queue = RelayQueue::default();
        let t0 = 3_000_000;
        queue.poll(t0);
        let id = queue.submit("read_conversation", "{}", t0).unwrap();
        queue.poll(t0);
        // The controller took it and never came back.
        assert!(!queue.result(&id, t0 + EXPIRY_MS - 1).unwrap().done);
        let error = queue.result(&id, t0 + EXPIRY_MS).unwrap_err();
        assert!(error.to_string().contains("did not answer"), "{error}");
        // And a controller that has not asked for work in a while is gone.
        assert!(queue.attached(t0 + ATTACHED_MS));
        assert!(!queue.attached(t0 + ATTACHED_MS + 1));
    }
}
