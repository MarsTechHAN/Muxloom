//! Moderators: agents muxloom runs to coordinate the other agents.
//!
//! A moderator is an ordinary Codex or Claude session on this machine. What
//! makes it a moderator is where it lives — a folder muxloom owns under its own
//! state directory — and what it finds there: a briefing naming it, saying which
//! machines and which agents it was asked to look after, and pointing it at the
//! `muxloom` MCP tools it coordinates them with.
//!
//! The scope is a briefing, not a sandbox. Nothing here narrows what the MCP
//! surface will answer: a moderator can reach every machine the user has
//! enabled, exactly like any other agent on this machine. Saying so plainly in
//! the briefing is the whole of the enforcement, and the launch form says the
//! same thing to the user.
//!
//! Nothing here registers those tools. The local daemon writes the `muxloom`
//! MCP entry and the skill into this user's Codex and Claude configuration when
//! it starts (see [`crate::mcp_register`]), and it has to be running before
//! anything can be launched at all — so a moderator inherits them the way every
//! other agent on this machine does. That is also why the launch form offers
//! only those two runtimes: a moderator that cannot call the tools has nothing
//! to moderate with.

use std::{
    fmt::Write as _,
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};

use crate::model::AgentKind;

/// The directory, under muxloom's own state, that holds one folder per
/// moderator. Named for what an agent sees rather than for what it is: the
/// moderator's working directory is a project like any other.
pub const PROJECTS_DIR: &str = "projects";

/// The file muxloom writes and rewrites. Both agent runtimes read a briefing
/// from their working directory, under different names, so both are written
/// with the same body.
const BRIEFING_FILES: [&str; 2] = ["CLAUDE.md", "AGENTS.md"];

/// What a moderator was asked to look after, as the launch form collected it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Moderator {
    /// What the user called it. Required, and what the session is labelled.
    pub name: String,
    pub kind: AgentKind,
    /// Machines in scope, by their dashboard label. Empty means every machine.
    pub machines: Vec<String>,
    /// Agents in scope, as one readable line each. Empty means every agent.
    pub agents: Vec<String>,
}

impl Moderator {
    /// The folder name: the moderator's name reduced to something safe to put
    /// in a path and to type at a shell.
    pub fn slug(&self) -> String {
        slug(&self.name)
    }
}

/// Reduce a name to lowercase words joined by dashes. Anything that is not a
/// letter or a digit separates words, so a name in any script that has no such
/// characters reduces to nothing — the caller falls back to a fixed name.
pub fn slug(name: &str) -> String {
    let mut out = String::new();
    let mut pending_dash = false;
    for character in name.chars() {
        if character.is_alphanumeric() {
            if pending_dash && !out.is_empty() {
                out.push('-');
            }
            pending_dash = false;
            out.extend(character.to_lowercase());
        } else {
            pending_dash = true;
        }
    }
    if out.is_empty() {
        "moderator".into()
    } else {
        out
    }
}

/// Where every moderator's folder lives, given muxloom's state directory.
pub fn root(state_dir: &Path) -> PathBuf {
    state_dir.join(PROJECTS_DIR)
}

/// Whether a session's working directory is one of these folders, which is what
/// makes it a moderator rather than an ordinary local agent. Compared as text:
/// the path arrives from the daemon as a string, and the folder may not exist
/// any more by the time an archived session is listed.
pub fn is_moderator_path(state_dir: &Path, path: &str) -> bool {
    let root = root(state_dir);
    let root = root.to_string_lossy();
    let root = root.trim_end_matches('/');
    let path = path.trim_end_matches('/');
    path.strip_prefix(root)
        .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
}

/// Create this moderator's folder and leave the briefing in it, returning the
/// folder. Rewritten on every launch: the moderator may have been given a
/// different scope than the one it ran with last time.
pub fn prepare(state_dir: &Path, moderator: &Moderator) -> Result<PathBuf> {
    let folder = root(state_dir).join(moderator.slug());
    fs::create_dir_all(&folder)
        .with_context(|| format!("failed to create {}", folder.display()))?;
    let briefing = briefing(moderator);
    for name in BRIEFING_FILES {
        let path = folder.join(name);
        fs::write(&path, &briefing)
            .with_context(|| format!("failed to write {}", path.display()))?;
    }
    Ok(folder)
}

/// What the moderator reads when it starts: who it is, who it is looking after,
/// how to reach them, and what it must not decide on its own.
fn briefing(moderator: &Moderator) -> String {
    let mut text = format!(
        "# {name} — a muxloom moderator\n\
         \n\
         You are **{name}**, an agent muxloom runs to coordinate other agents.\n\
         You have no code of your own to write here. Your work is to understand\n\
         what your user wants, decide who should do it, hand it out, follow it\n\
         up, and report back.\n\
         \n\
         This folder is yours: muxloom made it for you, and nothing else lives\n\
         in it. Keep notes here if they help you, but the shared record belongs\n\
         on the talk board, where the others can read it.\n\
         \n",
        name = moderator.name
    );

    text.push_str("## What you were asked to look after\n\n");
    text.push_str("Machines:\n\n");
    if moderator.machines.is_empty() {
        text.push_str("- Every machine the user has enabled.\n");
    } else {
        for machine in &moderator.machines {
            let _ = writeln!(text, "- {machine}");
        }
    }
    text.push_str("\nAgents:\n\n");
    if moderator.agents.is_empty() {
        text.push_str("- Every agent on those machines, including ones started after you.\n");
    } else {
        for agent in &moderator.agents {
            let _ = writeln!(text, "- {agent}");
        }
    }
    text.push_str(
        "\n\
         This is a brief, not a fence. The muxloom tools will answer for\n\
         machines and sessions outside it, and will not stop you. Staying inside\n\
         it is your judgement: if the work needs someone who is not on the list,\n\
         ask your user before you go and get them.\n\
         \n",
    );

    text.push_str(
        "## How to run the work\n\
         \n\
         1. `talk_read {}` first, every time you pick something up. The board\n\
            carries what every other agent and every person at a dashboard is\n\
            doing, on every machine.\n\
         2. `list_sessions {}` to see who exists and whether they are working,\n\
            idle, or waiting on a human.\n\
         3. `message_agent { session_id, text }` to hand out a task. It lands in\n\
            that session's prompt in an envelope naming you, so the agent knows\n\
            it is hearing from a colleague and not from its user. Say what you\n\
            want and what you already know; one message, not a stream.\n\
         4. `talk_read { scope: \"direct\", wait_seconds: 120 }` to hear back,\n\
            and `wait_for { session_id, until: \"idle\" }` to watch a session\n\
            rather than poll it.\n\
         5. `launch_session { kind, path, label }` when nobody suitable exists.\n\
            Give it a name you will recognise later, and tell it who else is on\n\
            the same problem — agents do not discover each other on their own.\n\
         6. `talk_post { text, kind: \"note\" }` for what you worked out. Your\n\
            context ends with this conversation; the note does not.\n\
         \n\
         Nobody here reports to you. A message from you is a request an agent\n\
         may refuse, and it is already in the middle of something you cannot\n\
         see. Ask, do not order, and check before you interrupt.\n\
         \n\
         ## Ask your user first\n\
         \n\
         - Deleting a session, or stopping work you did not start.\n\
         - Enabling or disabling a machine, or editing SSH configuration.\n\
         - Anything on a machine or with an agent outside the brief above.\n\
         - Any target you had to guess at.\n",
    );
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let path =
            std::env::temp_dir().join(format!("muxloom-moderator-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn a_name_becomes_a_folder_that_is_safe_to_type() {
        assert_eq!(slug("Fleet Lead"), "fleet-lead");
        assert_eq!(slug("  release/2026-Q3  "), "release-2026-q3");
        assert_eq!(slug("../../etc/passwd"), "etc-passwd");
        // A name with nothing to reduce still has to land somewhere.
        assert_eq!(slug("统筹"), "统筹");
        assert_eq!(slug("!!!"), "moderator");
    }

    #[test]
    fn the_briefing_names_the_scope_and_says_it_is_not_enforced() {
        let moderator = Moderator {
            name: "Fleet Lead".into(),
            kind: AgentKind::Claude,
            machines: vec!["gpu-1".into()],
            agents: vec!["claude · ~/Works/Terminal".into()],
        };
        let state = scratch("briefing");
        let folder = prepare(&state, &moderator).unwrap();
        assert_eq!(folder, state.join("projects/fleet-lead"));

        let text = fs::read_to_string(folder.join("CLAUDE.md")).unwrap();
        assert_eq!(text, fs::read_to_string(folder.join("AGENTS.md")).unwrap());
        assert!(text.contains("You are **Fleet Lead**"), "{text}");
        assert!(text.contains("- gpu-1"), "{text}");
        assert!(text.contains("- claude · ~/Works/Terminal"), "{text}");
        // The one thing the user cannot see for themselves: the scope is
        // advisory, and the briefing has to admit it.
        assert!(text.contains("This is a brief, not a fence."), "{text}");
        for tool in ["talk_read", "message_agent", "launch_session", "wait_for"] {
            assert!(text.contains(tool), "the briefing never mentions {tool}");
        }
        let _ = fs::remove_dir_all(&state);
    }

    #[test]
    fn an_empty_scope_reads_as_everything() {
        let state = scratch("everything");
        let folder = prepare(
            &state,
            &Moderator {
                name: "all hands".into(),
                kind: AgentKind::Codex,
                machines: Vec::new(),
                agents: Vec::new(),
            },
        )
        .unwrap();
        let text = fs::read_to_string(folder.join("AGENTS.md")).unwrap();
        assert!(
            text.contains("Every machine the user has enabled."),
            "{text}"
        );
        assert!(text.contains("Every agent on those machines"), "{text}");
        let _ = fs::remove_dir_all(&state);
    }

    #[test]
    fn a_relaunch_rewrites_the_brief_it_runs_with() {
        let state = scratch("relaunch");
        let mut moderator = Moderator {
            name: "lead".into(),
            kind: AgentKind::Claude,
            machines: vec!["gpu-1".into()],
            agents: Vec::new(),
        };
        prepare(&state, &moderator).unwrap();
        moderator.machines = vec!["mars-mbp".into()];
        let folder = prepare(&state, &moderator).unwrap();
        let text = fs::read_to_string(folder.join("CLAUDE.md")).unwrap();
        assert!(text.contains("- mars-mbp"), "{text}");
        assert!(!text.contains("- gpu-1"), "{text}");
        let _ = fs::remove_dir_all(&state);
    }

    #[test]
    fn only_folders_under_the_projects_directory_are_moderators() {
        let state = Path::new("/home/me/.local/state/muxloom");
        assert!(is_moderator_path(
            state,
            "/home/me/.local/state/muxloom/projects/lead"
        ));
        assert!(is_moderator_path(
            state,
            "/home/me/.local/state/muxloom/projects/lead/"
        ));
        assert!(is_moderator_path(
            state,
            "/home/me/.local/state/muxloom/projects"
        ));
        assert!(!is_moderator_path(state, "/home/me/Works/Terminal"));
        // A sibling directory whose name merely starts the same way is not one.
        assert!(!is_moderator_path(
            state,
            "/home/me/.local/state/muxloom/projects-old/lead"
        ));
    }
}
