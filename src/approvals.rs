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

    /// Park a new ask under a fresh id and return it. Caller writes the file.
    pub fn park(&mut self, id: String, pending: Pending) {
        self.pending.insert(id, pending);
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

    /// An id the witness never touches, so two asks cannot share bookkeeping.
    pub fn fresh_id(counter: &mut u64) -> String {
        *counter += 1;
        format!("approve-{}", *counter)
    }
}

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
