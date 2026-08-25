//! Channels: the ways muxloom reaches the human who is not at a dashboard.
//!
//! A binding is one chat app plus the credentials to post into one
//! conversation with it. The dashboard is where they are written down, but the
//! sending is not the dashboard's job: an agent that finishes something on a
//! remote machine at three in the morning tells the daemon it lives in, and
//! that daemon posts. So the whole set — secrets included — is pushed to every
//! enabled machine on the same round that carries the talk board, and each
//! daemon keeps its copy in a `0600` file beside its sessions.
//!
//! Two things deliberately never carry a secret:
//!
//! - the talk board, because every agent can read it, and
//! - the example config, because that file ends up in dotfile repositories.
//!
//! An agent names a channel by its [`ChannelBinding::id`] and is never shown
//! what is behind it.

use std::{
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::{debug, model::Target, runtime::Runtime};

/// Advertised by a daemon that can be given a channel set. A daemon without it
/// is simply left out of the round; nothing else about it changes.
pub const CHANNELS_CAPABILITY: &str = "channels-v1";

/// The file every machine keeps its copy in, relative to its state directory.
pub const CHANNELS_FILE: &str = "channels.json";

/// Which chat app a binding speaks.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChannelKind {
    /// Lark / 飞书, through a custom app: `app_id` + `app_secret`, posting into
    /// one chat. The only kind a human can also answer through.
    #[default]
    Lark,
    /// WeCom / 企业微信 group robot: one webhook key, and nothing comes back.
    /// Personal WeChat has no API at all, so it is not offered.
    WeCom,
}

impl ChannelKind {
    pub const ALL: [Self; 2] = [Self::Lark, Self::WeCom];

    /// The word used in ids and on the wire.
    pub fn slug(self) -> &'static str {
        match self {
            Self::Lark => "lark",
            Self::WeCom => "wecom",
        }
    }

    /// What a person calls it, in both the names they might look for.
    pub fn title(self) -> &'static str {
        match self {
            Self::Lark => "Lark / 飞书",
            Self::WeCom => "WeCom / 企业微信",
        }
    }

    /// Whether a human's reply can find its way back to an agent this way. A
    /// group robot is a loudspeaker: it has no read side, and the panel says so
    /// rather than letting someone wait for an answer that cannot arrive.
    pub fn listens(self) -> bool {
        matches!(self, Self::Lark)
    }
}

/// One place to write to, and what it takes to write there.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChannelBinding {
    /// Stable and short (`lark-1`). This is the only part of a binding an agent
    /// ever names, and it is what a tool call carries.
    pub id: String,
    #[serde(default)]
    pub kind: ChannelKind,
    /// What the panel shows, and what a message says it came through.
    #[serde(default)]
    pub label: String,
    /// Lark: the app id (`cli_…`). Unused by WeCom.
    #[serde(default)]
    pub app_id: String,
    /// Lark: the app secret. WeCom: the group robot key out of its webhook URL.
    /// The one field that must not appear anywhere a reader could be an agent.
    #[serde(default)]
    pub secret: String,
    /// Lark: the chat id (`oc_…`) to post into. Unused by WeCom, whose webhook
    /// already names the group.
    #[serde(default)]
    pub route: String,
    /// Whether a message that names no channel goes here.
    #[serde(default)]
    pub preferred: bool,
}

impl ChannelBinding {
    /// A one-line description for a panel or an error, with nothing secret in
    /// it.
    pub fn describes(&self) -> String {
        let where_to = match self.kind {
            ChannelKind::Lark if !self.route.is_empty() => format!(" · {}", self.route),
            ChannelKind::Lark => " · no chat yet".into(),
            ChannelKind::WeCom => " · group robot".into(),
        };
        format!("{}{where_to}", self.kind.title())
    }

    /// Whether this binding has everything it needs to be used, and what is
    /// missing when it does not.
    pub fn ready(&self) -> Result<()> {
        let missing: &[&str] = match self.kind {
            ChannelKind::Lark => &[
                if self.app_id.trim().is_empty() {
                    "app id"
                } else {
                    ""
                },
                if self.secret.trim().is_empty() {
                    "app secret"
                } else {
                    ""
                },
                if self.route.trim().is_empty() {
                    "chat id"
                } else {
                    ""
                },
            ],
            ChannelKind::WeCom => &[if self.secret.trim().is_empty() {
                "webhook key"
            } else {
                ""
            }],
        };
        let missing: Vec<&str> = missing
            .iter()
            .copied()
            .filter(|it| !it.is_empty())
            .collect();
        if missing.is_empty() {
            return Ok(());
        }
        bail!(
            "channel {} is missing its {}",
            self.id,
            missing.join(" and its ")
        )
    }

    /// The same binding with the secret taken out, for anything that leaves
    /// this process other than a push to one of muxloom's own daemons.
    pub fn redacted(&self) -> Self {
        Self {
            secret: String::new(),
            ..self.clone()
        }
    }
}

/// Every binding this fleet knows about, and which version of the list it is.
///
/// The revision is what makes the push cheap and the ordering unambiguous: the
/// dashboard is the only writer, so a daemon holding the same number is holding
/// the same list and can be skipped, and a daemon holding a larger one is
/// holding something a newer dashboard wrote and is left alone.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChannelSet {
    #[serde(default)]
    pub revision: u64,
    #[serde(default)]
    pub bindings: Vec<ChannelBinding>,
}

impl ChannelSet {
    /// Read a set, treating a file that is not there as an empty one: a machine
    /// nobody has bound a channel on is not a machine in error.
    pub fn load(path: &Path) -> Result<Self> {
        let text = match fs::read_to_string(path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::default());
            }
            Err(error) => {
                return Err(error).with_context(|| format!("failed to read {}", path.display()));
            }
        };
        serde_json::from_str(&text)
            .with_context(|| format!("invalid channels in {}", path.display()))
    }

    /// The same, for a daemon: a file it cannot parse is not worth refusing to
    /// serve over, it just means nothing can be sent until the next push.
    pub fn read(path: &Path) -> Self {
        Self::load(path).unwrap_or_else(|error| {
            eprintln!("muxloomd could not read {}: {error:#}", path.display());
            Self::default()
        })
    }

    /// Write the set where only this user can read it. Written to one side and
    /// renamed, so a reader never sees half a file.
    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        let temporary = path.with_extension("json.tmp");
        let text = serde_json::to_string_pretty(self)?;
        fs::write(&temporary, format!("{text}\n"))
            .with_context(|| format!("failed to write {}", temporary.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&temporary, fs::Permissions::from_mode(0o600))?;
        }
        fs::rename(&temporary, path)
            .with_context(|| format!("failed to replace {}", path.display()))
    }

    pub fn is_empty(&self) -> bool {
        self.bindings.is_empty()
    }

    pub fn find(&self, id: &str) -> Option<&ChannelBinding> {
        self.bindings.iter().find(|binding| binding.id == id)
    }

    /// Which binding a message goes to: the one it names, or the preferred one,
    /// or the only one there is.
    pub fn pick(&self, id: Option<&str>) -> Result<&ChannelBinding> {
        if let Some(id) = id.map(str::trim).filter(|id| !id.is_empty()) {
            return self.find(id).with_context(|| {
                format!(
                    "no channel called {id}. This machine knows: {}",
                    self.names()
                )
            });
        }
        if let Some(binding) = self.bindings.iter().find(|binding| binding.preferred) {
            return Ok(binding);
        }
        match self.bindings.as_slice() {
            [] => bail!(
                "no channel is bound. A human sets one up in the muxloom dashboard: \
                 select the machines panel and press c."
            ),
            [only] => Ok(only),
            _ => bail!(
                "several channels are bound and none is the default; name one of: {}",
                self.names()
            ),
        }
    }

    /// The ids, for an error that has to say what the alternatives were.
    pub fn names(&self) -> String {
        if self.bindings.is_empty() {
            return "(none)".into();
        }
        self.bindings
            .iter()
            .map(|binding| binding.id.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    }

    /// The set with every secret taken out.
    pub fn redacted(&self) -> Self {
        Self {
            revision: self.revision,
            bindings: self.bindings.iter().map(ChannelBinding::redacted).collect(),
        }
    }

    /// An id nothing else in the set is using.
    pub fn mint_id(&self, kind: ChannelKind) -> String {
        (1..)
            .map(|number| format!("{}-{number}", kind.slug()))
            .find(|candidate| self.find(candidate).is_none())
            .unwrap_or_else(|| kind.slug().to_string())
    }

    /// Take a pushed set if it is at least as new as what is held. Returns
    /// whether anything changed, so a daemon only writes the file when it did.
    pub fn adopt(&mut self, incoming: Self) -> bool {
        if incoming.revision < self.revision || incoming == *self {
            return false;
        }
        *self = incoming;
        true
    }
}

/// Where a machine keeps its copy, given the directory it keeps its state in.
pub fn path_in(state_dir: &Path) -> PathBuf {
    state_dir.join(CHANNELS_FILE)
}

/// What one push round did, for the debug log and for the panel's count.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ChannelRound {
    /// Machines that now hold this revision, this one included.
    pub synced: usize,
    /// Machines that were asked, whether or not they answered.
    pub asked: usize,
    /// Machines that could not be reached or refused, with the reason.
    pub failures: Vec<(String, String)>,
}

impl ChannelRound {
    pub fn complete(&self) -> bool {
        self.failures.is_empty() && self.synced == self.asked
    }
}

/// Push the dashboard's channel set to every machine that can hold one.
///
/// Each machine is asked what it has first. That one small question is what
/// makes the round self-correcting: a machine that was off when the set changed,
/// or enabled afterwards, or rebuilt from nothing, catches up on the next round
/// without anyone remembering that it needs to. A machine whose daemon predates
/// channels is skipped in silence — there is nothing it could do with the set.
pub fn run_sync(runtime: &Runtime, targets: &[Target], set: &ChannelSet) -> ChannelRound {
    let mut round = ChannelRound::default();
    if set.is_empty() && set.revision == 0 {
        // Nothing has ever been bound, so there is nothing to keep in step and
        // no reason to knock on every machine every few seconds.
        return round;
    }
    let pool = runtime.bridge_pool();
    let local = Target::local();
    let everywhere = std::iter::once(&local).chain(targets.iter().filter(|it| it.id != local.id));
    for target in everywhere {
        match pool.channels_get(target) {
            Ok(None) => continue,
            Ok(Some(held)) => {
                round.asked += 1;
                if held.revision == set.revision {
                    round.synced += 1;
                    continue;
                }
                match pool.channels_put(target, set) {
                    Ok(()) => round.synced += 1,
                    Err(error) => round
                        .failures
                        .push((target.id.clone(), format!("{error:#}"))),
                }
            }
            Err(error) => {
                round.asked += 1;
                round
                    .failures
                    .push((target.id.clone(), format!("{error:#}")));
            }
        }
    }
    for (machine, error) in &round.failures {
        debug::log("channel", format!("{machine}: not in step ({error})"));
    }
    round
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lark(id: &str) -> ChannelBinding {
        ChannelBinding {
            id: id.into(),
            kind: ChannelKind::Lark,
            label: "Team".into(),
            app_id: "cli_9".into(),
            secret: "shhh".into(),
            route: "oc_1".into(),
            preferred: false,
        }
    }

    #[test]
    fn a_set_survives_the_round_trip_through_a_private_file() {
        let path = std::env::temp_dir().join(format!(
            "muxloom-channels-{}-{}.json",
            std::process::id(),
            line!()
        ));
        let set = ChannelSet {
            revision: 3,
            bindings: vec![lark("lark-1")],
        };
        set.save(&path).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "a secret must not be readable by anyone else");
        }
        assert_eq!(ChannelSet::load(&path).unwrap(), set);
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn a_missing_file_is_an_empty_set_rather_than_an_error() {
        let path = std::env::temp_dir().join("muxloom-channels-not-here.json");
        let _ = fs::remove_file(&path);
        assert_eq!(ChannelSet::load(&path).unwrap(), ChannelSet::default());
    }

    #[test]
    fn a_daemon_takes_a_newer_list_and_keeps_a_newer_one_of_its_own() {
        let mut held = ChannelSet {
            revision: 2,
            bindings: vec![lark("lark-1")],
        };
        assert!(
            !held.adopt(ChannelSet {
                revision: 1,
                bindings: Vec::new()
            }),
            "an older push must not empty a machine"
        );
        assert!(
            !held.adopt(held.clone()),
            "the same list again is not a change to write down"
        );
        assert!(held.adopt(ChannelSet {
            revision: 3,
            bindings: vec![lark("lark-1"), lark("lark-2")],
        }));
        assert_eq!(held.bindings.len(), 2);
    }

    #[test]
    fn picking_prefers_the_name_then_the_default_then_the_only_one() {
        let mut set = ChannelSet {
            revision: 1,
            bindings: vec![lark("lark-1")],
        };
        assert_eq!(set.pick(None).unwrap().id, "lark-1");
        assert_eq!(set.pick(Some("lark-1")).unwrap().id, "lark-1");
        let missing = set.pick(Some("lark-9")).unwrap_err().to_string();
        assert!(
            missing.contains("lark-9") && missing.contains("lark-1"),
            "an unknown channel must say what there is instead: {missing}"
        );

        set.bindings.push(ChannelBinding {
            id: "wecom-1".into(),
            kind: ChannelKind::WeCom,
            secret: "key".into(),
            ..Default::default()
        });
        let ambiguous = set.pick(None).unwrap_err().to_string();
        assert!(ambiguous.contains("lark-1") && ambiguous.contains("wecom-1"));
        set.bindings[1].preferred = true;
        assert_eq!(set.pick(None).unwrap().id, "wecom-1");

        let empty = ChannelSet::default();
        assert!(
            empty
                .pick(None)
                .unwrap_err()
                .to_string()
                .contains("press c")
        );
    }

    #[test]
    fn nothing_that_leaves_for_a_reader_carries_the_secret() {
        let set = ChannelSet {
            revision: 1,
            bindings: vec![lark("lark-1")],
        };
        let redacted = set.redacted();
        assert_eq!(redacted.revision, 1);
        assert!(redacted.bindings[0].secret.is_empty());
        assert_eq!(redacted.bindings[0].app_id, "cli_9");
        assert!(!set.bindings[0].describes().contains("shhh"));
        assert!(
            !serde_json::to_string(&redacted).unwrap().contains("shhh"),
            "a redacted set must be safe to hand to whoever asked"
        );
    }

    #[test]
    fn a_new_id_never_collides_with_one_in_use() {
        let set = ChannelSet {
            revision: 1,
            bindings: vec![lark("lark-1"), lark("lark-2")],
        };
        assert_eq!(set.mint_id(ChannelKind::Lark), "lark-3");
        assert_eq!(set.mint_id(ChannelKind::WeCom), "wecom-1");
    }

    #[test]
    fn an_incomplete_binding_says_which_field_is_missing() {
        let mut binding = lark("lark-1");
        binding.route.clear();
        let error = binding.ready().unwrap_err().to_string();
        assert!(error.contains("chat id"), "{error}");
        binding.secret.clear();
        let error = binding.ready().unwrap_err().to_string();
        assert!(
            error.contains("app secret") && error.contains("chat id"),
            "{error}"
        );

        let robot = ChannelBinding {
            id: "wecom-1".into(),
            kind: ChannelKind::WeCom,
            secret: "key".into(),
            ..Default::default()
        };
        assert!(robot.ready().is_ok(), "a robot needs nothing but its key");
    }
}
