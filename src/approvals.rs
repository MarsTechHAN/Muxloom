//! Cross-machine write approvals.
//!
//! A daemon-flavoured agent reaches another machine only by relaying through
//! an attached muxloom controller. muxloom's relay deliberately lets only
//! look-and-speak tools through (see `relay::RELAYED_TOOLS`); a WRITE tool —
//! starting a session, typing into one, deleting one — must not run on a
//! remote machine without a person saying so. That person is usually not at
//! the dashboard, so the ask goes out over the bound chat, and the answer
//! comes back through the same chat.
//!
//! This module owns the ledger that ask leaves in. It is per-daemon and
//! persisted to `<state>/approvals.json`, because the human takes far longer
//! to answer than one relay round lasts, and a daemon that restarts between
//! the ask and the answer should still know what it was waiting on.
//!
//! Three degrees of yes, modelled on the way a CLI lets you keep working:
//!   - one-shot: run it this once;
//!   - always-for-this-session: remember `(session, machine, tool)` for the
//!     rest of the conversation (only for the less dangerous tools);
//!   - this-time-no: refuse it once and remember nothing.
//!
//! `set_machine_enabled` and `ssh_host` are never here, on either side of the
//! gate: a machine an agent could not be lawfully on must not become lawful
//! because the agent asked nicely. They stay forbidden to relay entirely.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// One cross-machine WRITE a remote agent asked for and a person has not yet
/// answered.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pending {
    /// The originating session, whose identity the approval is scoped to.
    #[serde(default)]
    pub session: String,
    /// The machine the tool was aimed at.
    #[serde(default)]
    pub machine: String,
    /// The machine the asking session is on, which is rarely the one the write
    /// was aimed at. Without it an answer has nowhere to go: the person says
    /// yes, and the agent waiting on it is never told. Empty in a ledger
    /// written before this was recorded.
    #[serde(default)]
    pub origin: String,
    /// The tool name (e.g. "launch_session").
    pub tool: String,
    /// A short human-readable description of what was asked, shown to the
    /// person instead of a raw argument blob.
    #[serde(default)]
    pub ask: String,
    /// When the ask went out.
    pub at_ms: u64,
    /// Whether this pending entry has not yet been decided.
    #[serde(default = "default_true")]
    pub open: bool,
}

fn default_true() -> bool {
    true
}

/// Everything the approval gate remembers. Written atomically (tmp + rename)
/// and read whole.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Approvals {
    /// Pending asks, keyed by the id a chat reply names.
    #[serde(default)]
    pub pending: HashMap<String, Pending>,
    /// "Always for this conversation": `(session, machine, tool)` triples the
    /// person already said yes to, remembered until the ledger is reset.
    #[serde(default)]
    pub allow: HashMap<String, Vec<(String, String, String)>>,
    /// One-shot grants: `(session, machine, tool)` a person approved exactly
    /// once. The next matching job runs and then the grant is spent.
    #[serde(default)]
    pub once: Vec<(String, String, String)>,
    /// The highest number this ledger has ever put on an ask. Kept in the file
    /// rather than in the process, so a restart carries on counting.
    #[serde(default)]
    pub minted: u64,
}

/// The number an id names, when it is one of ours.
fn id_number(id: &str) -> Option<u64> {
    id.strip_prefix("approve-")?.parse().ok()
}

impl Approvals {
    pub fn load(path: &Path) -> Self {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        let temporary = path.with_extension("json.tmp");
        std::fs::write(
            &temporary,
            format!("{}\n", serde_json::to_string_pretty(self)?),
        )
        .with_context(|| format!("failed to write {}", temporary.display()))?;
        std::fs::rename(&temporary, path)
            .with_context(|| format!("failed to replace {}", path.display()))
    }

    pub fn approvals_path_in(state_dir: &Path) -> PathBuf {
        state_dir.join("approvals.json")
    }

    /// The approvals document under the default muxloom state directory — the
    /// one the controller and the local daemon both keep their other files in.
    /// The controller is the only reader and writer of this ledger, so this
    /// needs no discovery fancier than the default home path.
    pub fn default_path() -> PathBuf {
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
        Self::approvals_path_in(&Path::new(&home).join(".local/state/muxloom"))
    }

    /// True when the person has already said "yes for the rest of this
    /// conversation" to `(session, machine, tool)`.
    pub fn remembered(&self, session: &str, machine: &str, tool: &str) -> bool {
        self.once
            .iter()
            .any(|(s, m, t)| s == session && m == machine && t == tool)
            || self
                .allow
                .get(session)
                .is_some_and(|list| list.iter().any(|(m, t, _)| m == machine && t == tool))
    }

    /// The ask this session already has open for the same write on the same
    /// machine, if there is one.
    ///
    /// A person answers in their own time, and the agent will try the call
    /// again while they are deciding. Every one of those retries has to land
    /// on the ask already in front of them: without this each attempt mints
    /// another id and puts another copy on their phone, and a person answering
    /// the first one leaves all the others open forever.
    ///
    /// Only asks young enough to still be waiting on somebody are here to be
    /// found; `forget_stale` is what takes the others out of the way.
    pub fn open_ask(&self, session: &str, machine: &str, tool: &str) -> Option<&str> {
        self.pending
            .iter()
            .filter(|(_, p)| {
                p.open && p.session == session && p.machine == machine && p.tool == tool
            })
            // Oldest first, so a person looking at two asks is told about the
            // one they have been looking at longest. Map order is arbitrary.
            .min_by_key(|(id, p)| (p.at_ms, id.as_str()))
            .map(|(id, _)| id.as_str())
    }

    /// The number for the next ask, counted from the file.
    ///
    /// A person answers in hours and a controller restarts in minutes, so the
    /// two cannot share a counter that lives in the process. One that begins
    /// again at one hands `approve-3` to a second ask while the first
    /// `approve-3` is still on somebody's phone, and the yes they type then
    /// settles whichever of the two the ledger happens to be holding: a person
    /// reading one write and granting another. The floor is the highest number
    /// still parked as well as the recorded one, so a ledger written by a build
    /// that had no counter is not walked over either.
    ///
    /// Minting alone does not dirty the file. An ask that never reaches the
    /// person is never parked, and a number nobody was shown is free to be
    /// handed out again.
    pub fn mint(&mut self) -> u64 {
        let parked = self.pending.keys().filter_map(|id| id_number(id)).max();
        self.minted = self.minted.max(parked.unwrap_or(0)) + 1;
        self.minted
    }

    /// Park a new ask under `id`. Caller writes the file.
    ///
    /// False, and nothing written, when that id is already an ask waiting on an
    /// answer: overwriting one silently would leave the person looking at a
    /// card for a write that is no longer what saying yes to it does.
    pub fn park(&mut self, id: String, pending: Pending) -> bool {
        if self.pending.get(&id).is_some_and(|open| open.open) {
            return false;
        }
        self.pending.insert(id, pending);
        true
    }

    /// Remember `(session, machine, tool)` for the rest of the session.
    pub fn remember(&mut self, session: &str, machine: &str, tool: &str) -> bool {
        let entry = self.allow.entry(session.to_string()).or_default();
        if entry.iter().any(|(m, t, _)| m == machine && t == tool) {
            return false;
        }
        entry.push((machine.to_string(), tool.to_string(), String::new()));
        true
    }

    /// Grant `(session, machine, tool)` for exactly one run.
    pub fn grant_once(&mut self, session: &str, machine: &str, tool: &str) -> bool {
        if self
            .once
            .iter()
            .any(|(s, m, t)| s == session && m == machine && t == tool)
        {
            return false;
        }
        self.once
            .push((session.to_string(), machine.to_string(), tool.to_string()));
        true
    }

    /// Spend a one-shot grant after a granted job ran.
    pub fn spend_once(&mut self, session: &str, machine: &str, tool: &str) {
        self.once
            .retain(|(s, m, t)| !(s == session && m == machine && t == tool));
    }

    /// Resolve an id any way it is decided, removing the pending entry.
    pub fn take(&mut self, id: &str) -> Option<Pending> {
        self.pending.remove(id).filter(|p| p.open)
    }

    /// A person said no; the entry is gone and nothing is remembered.
    pub fn refuse(&mut self, id: &str) -> Option<Pending> {
        self.take(id)
    }

    /// Drop everything a session asked, and its remembered approvals, when the
    /// session ends. Returns how many pending asks were dropped.
    pub fn end_session(&mut self, session: &str) -> usize {
        self.allow.remove(session);
        self.once.retain(|(s, _, _)| s != session);
        let before = self.pending.len();
        self.pending.retain(|_, p| p.session != session);
        before - self.pending.len()
    }

    /// Drop asks nobody answered in `ASK_STANDS_MS`. Returns how many went.
    ///
    /// Without this an ask is open forever. The person scrolls past the card,
    /// the conversation it belonged to ends, and the entry stays — so every
    /// later try at the same write on the same machine by the same session is
    /// told, for as long as that session lives, that it is waiting on a
    /// question already days old and answerable only by finding a card nobody
    /// can see any more. Letting it lapse costs one more ask; keeping it costs
    /// the write, permanently.
    ///
    /// A day, because a person who has not looked by tomorrow is not about to,
    /// and because an answer that late lands on an agent that has long since
    /// stopped asking.
    pub fn forget_stale(&mut self, now: u64) -> usize {
        let before = self.pending.len();
        self.pending
            .retain(|_, ask| now.saturating_sub(ask.at_ms) < ASK_STANDS_MS);
        before - self.pending.len()
    }
}

/// How long an unanswered ask stands before it is treated as lapsed.
pub const ASK_STANDS_MS: u64 = 24 * 60 * 60 * 1_000;

/// Parse a person's chat reply to an approval ask.
///
/// Accepts the plain words and the numbered form the ask presents, and
/// lower-case aliases for a phone that cannot trust its case.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Run it this once.
    Yes,
    /// Run it now and remember it for the rest of this conversation.
    Always,
    /// Do not run it.
    No,
}

pub fn parse_verdict(reply: &str) -> Option<Verdict> {
    match reply.trim().to_ascii_lowercase().as_str() {
        "1" | "yes" | "y" | "approve" | "allow" => Some(Verdict::Yes),
        "2" | "3" | "always" | "always for this conversation" | "allow always" | "a" => {
            Some(Verdict::Always)
        }
        "0" | "no" | "n" | "reject" | "deny" | "never" => Some(Verdict::No),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn scratch(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("muxloom-approvals-{}-{name}", std::process::id()))
    }

    #[test]
    fn a_ledger_remembers_and_spends_one_shot_grants() {
        let mut a = Approvals::default();
        assert!(!a.remembered("s1", "seed", "launch_session"));
        a.remember("s1", "seed", "launch_session");
        assert!(a.remembered("s1", "seed", "launch_session"));
        // One shot is honoured and then spent.
        assert!(!a.remembered("s2", "seed", "send_input"));
        a.grant_once("s2", "seed", "send_input");
        assert!(a.remembered("s2", "seed", "send_input"));
        a.spend_once("s2", "seed", "send_input");
        assert!(!a.remembered("s2", "seed", "send_input"));
        // Ending a session forgets what it remembered, including oneshots.
        a.end_session("s1");
        assert!(!a.remembered("s1", "seed", "launch_session"));
    }

    /// The agent calls again while the person is still deciding. Without a
    /// look for the ask already open, every retry mints another id and puts
    /// another copy on their phone — and the one they answer leaves all the
    /// rest of them open forever.
    #[test]
    fn a_second_try_finds_the_ask_the_person_is_already_looking_at() {
        let mut a = Approvals::default();
        let ask = |session: &str, machine: &str, tool: &str, at_ms: u64| Pending {
            session: session.into(),
            machine: machine.into(),
            origin: "seed".into(),
            tool: tool.into(),
            ask: String::new(),
            at_ms,
            open: true,
        };
        assert_eq!(a.open_ask("s1", "seed", "launch_session"), None);
        assert!(a.park("approve-1".into(), ask("s1", "seed", "launch_session", 10)));
        assert_eq!(
            a.open_ask("s1", "seed", "launch_session"),
            Some("approve-1")
        );
        // Scoped to all three: another session, another machine, or another
        // tool is a different question and gets asked on its own.
        assert_eq!(a.open_ask("s2", "seed", "launch_session"), None);
        assert_eq!(a.open_ask("s1", "laptop", "launch_session"), None);
        assert_eq!(a.open_ask("s1", "seed", "run_shell"), None);
        // The oldest one is the one they have been looking at longest, and a
        // HashMap has no order of its own to fall back on.
        assert!(a.park("approve-2".into(), ask("s1", "seed", "launch_session", 5)));
        assert_eq!(
            a.open_ask("s1", "seed", "launch_session"),
            Some("approve-2")
        );
        // Answered is not open: the next call asks again rather than waiting
        // on a question that is already settled.
        a.take("approve-2");
        a.take("approve-1");
        assert_eq!(a.open_ask("s1", "seed", "launch_session"), None);
    }

    /// A controller restarts far more often than a person answers their phone.
    /// A number that begins again at one puts a second write behind a card the
    /// person is already looking at, and the yes they type grants whichever of
    /// the two the ledger is holding.
    #[test]
    fn a_number_is_never_handed_out_twice_however_often_the_controller_restarts() {
        let path = scratch("minting");
        let _ = std::fs::remove_file(&path);
        let ask = || Pending {
            session: "s1".into(),
            machine: "seed".into(),
            origin: "laptop".into(),
            tool: "launch_session".into(),
            ask: String::new(),
            at_ms: 1,
            open: true,
        };

        let mut a = Approvals::default();
        assert_eq!((a.mint(), a.mint(), a.mint()), (1, 2, 3));
        assert!(a.park("approve-3".into(), ask()));
        a.save(&path).unwrap();

        // The restart: a whole new ledger, read off the disk, carries on where
        // the counter left off rather than back at one.
        let mut back = Approvals::load(&path);
        assert_eq!(back.mint(), 4);

        // And a file written by a build that kept no counter at all still puts
        // the floor above every ask still parked in it.
        let mut older = Approvals {
            minted: 0,
            ..Default::default()
        };
        assert!(older.park("approve-9".into(), ask()));
        assert_eq!(older.mint(), 10);

        // An id still waiting on an answer is never quietly written over; one
        // that has been settled leaves its number free to be filed again.
        assert!(!older.park("approve-9".into(), ask()));
        older.take("approve-9");
        assert!(older.park("approve-9".into(), ask()));

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(path.with_extension("json.tmp"));
    }

    /// The ask that was never answered used to block its own write forever:
    /// `open_ask` kept finding it, and every retry for days afterwards was told
    /// to go and wait on a card nobody could still see.
    #[test]
    fn an_ask_nobody_answered_in_a_day_stops_standing_in_the_way() {
        let mut a = Approvals::default();
        let ask = |at_ms: u64| Pending {
            session: "s1".into(),
            machine: "seed".into(),
            origin: "laptop".into(),
            tool: "launch_session".into(),
            ask: String::new(),
            at_ms,
            open: true,
        };
        let now = 10 * ASK_STANDS_MS;
        assert!(a.park("approve-1".into(), ask(now - ASK_STANDS_MS)));
        assert!(a.park("approve-2".into(), ask(now - ASK_STANDS_MS / 2)));
        // A clock that disagrees with ours by running ahead is not grounds for
        // throwing an ask away.
        assert!(a.park("approve-3".into(), ask(now + 60_000)));

        assert_eq!(a.forget_stale(now), 1);
        assert_eq!(a.pending.len(), 2);
        assert!(!a.pending.contains_key("approve-1"));

        // The one still standing is still what a retry is told to wait on, and
        // once the day is up on that one too the next try asks afresh.
        assert_eq!(
            a.open_ask("s1", "seed", "launch_session"),
            Some("approve-2")
        );
        assert_eq!(a.forget_stale(now + 2 * ASK_STANDS_MS), 2);
        assert_eq!(a.open_ask("s1", "seed", "launch_session"), None);
    }

    #[test]
    fn verdicts_are_case_insensitive_and_numbers_agree() {
        assert_eq!(parse_verdict("approve"), Some(Verdict::Yes));
        assert_eq!(parse_verdict("1"), Some(Verdict::Yes));
        assert_eq!(parse_verdict("APPROVE"), Some(Verdict::Yes));
        assert_eq!(parse_verdict("always"), Some(Verdict::Always));
        assert_eq!(parse_verdict("reject"), Some(Verdict::No));
        assert_eq!(parse_verdict("0"), Some(Verdict::No));
        assert_eq!(parse_verdict("maybe"), None);
    }

    #[test]
    fn the_ledger_round_trips_through_a_private_file() {
        let path = scratch("roundtrip");
        let _ = std::fs::remove_file(&path);
        let mut a = Approvals::default();
        a.park(
            "approve-1".into(),
            Pending {
                session: "s1".into(),
                machine: "seed".into(),
                origin: "laptop".into(),
                tool: "launch_session".into(),
                ask: "run launch_session on seed".into(),
                at_ms: 1,
                open: true,
            },
        );
        a.save(&path).unwrap();
        let mut back = Approvals::load(Path::new(&path));
        assert!(back.take("approve-1").is_some());
        assert!(back.take("approve-1").is_none(), "taken once");
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(path.with_extension("json.tmp"));
    }
}
