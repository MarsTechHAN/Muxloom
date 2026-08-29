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
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
    sync::{Mutex, OnceLock},
    time::{Duration, Instant},
};

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{
    debug, http, ilink,
    model::{AgentKind, Target},
    runtime::Runtime,
};

/// Advertised by a daemon that can be given a channel set. A daemon without it
/// is simply left out of the round; nothing else about it changes.
pub const CHANNELS_CAPABILITY: &str = "channels-v1";

/// The file every machine keeps its copy in, relative to its state directory.
pub const CHANNELS_FILE: &str = "channels.json";

/// The board path the account lease lives on. Nothing ever works in that
/// directory, so a lease note sits off every human's and agent's default board
/// read, and an old muxloom that meets one just sees a note in an empty folder.
pub const LEASE_PATH: &str = "/muxloom/channel-leases";

/// The first line of a lease note. After it come `account=` (the WeChat
/// account, the bot id), `until=` (epoch-millisecond expiry), and `cursor=`
/// (the account cursor the holder has just advanced to, so a successor can
/// resume past what was already read). The holder is not written; the note's
/// origin is the machine that posted it, which is the only name both sides
/// agree on.
pub const LEASE_PREFIX: &str = "muxloom-channel-lease v1";

/// How long a claim stays valid. It must ride out a few missed polls and a
/// slow sync round, yet stay short enough that a machine whose controller died
/// stops being counted on within a minute.
pub const LEASE_TTL_MS: u64 = 45_000;

/// How long a machine that just posted its lease stays silent. It must ride
/// out the sync round to the other side (they run every couple of seconds),
/// so two simultaneous first claims settle on the smaller origin before
/// either one answers.
pub const LEASE_SETTLE_MS: u64 = 10_000;

/// How recently a target must have answered the channel round to count as a
/// live peer. A round knocks on every enabled target about every two seconds,
/// so thirty seconds is fifteen missed knocks.
pub const PEER_LIVE_WINDOW: Duration = Duration::from_secs(30);

/// When each target last answered the channel round. Process-local: the lease
/// itself is what travels between machines, this only says how fresh the world
/// is right now.
fn last_reach() -> &'static Mutex<HashMap<String, Instant>> {
    static LAST_REACH: OnceLock<Mutex<HashMap<String, Instant>>> = OnceLock::new();
    LAST_REACH.get_or_init(Default::default)
}

fn heard_from(target: &Target) {
    let map = last_reach();
    let mut guard = map.lock().unwrap_or_else(|poison| poison.into_inner());
    guard.insert(target.id.clone(), Instant::now());
}

fn no_longer_heard(target: &Target) {
    let map = last_reach();
    let mut guard = map.lock().unwrap_or_else(|poison| poison.into_inner());
    guard.remove(&target.id);
}

/// Whether any of the fleet's targets answered the channel round recently:
/// that is the 连着 signal. With no live peer the board cannot carry the other
/// side's word, so no lease of anyone else can be trusted either.
fn peers_are_live(targets: &[Target]) -> bool {
    let map = last_reach();
    let guard = map.lock().unwrap_or_else(|poison| poison.into_inner());
    targets.iter().any(|target| {
        guard
            .get(&target.id)
            .is_some_and(|at| at.elapsed() < PEER_LIVE_WINDOW)
    })
}

/// Which chat app a binding speaks.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum ChannelKind {
    /// WeChat / 微信, through a ClawBot. Scanned into being with the phone
    /// that is already in the room — no console to open, no app to register,
    /// nothing to copy across — which is why it is the one offered first.
    #[default]
    #[serde(rename = "wechat")]
    WeChat,
    /// Lark / 飞书, through a custom app: an app id and secret out of the open
    /// platform, posting into one chat. More to set up, but it is where a team
    /// already talks, and a group can have several people in it.
    #[serde(rename = "lark")]
    Lark,
}

impl ChannelKind {
    pub const ALL: [Self; 2] = [Self::WeChat, Self::Lark];

    /// The word used in ids and on the wire.
    pub fn slug(self) -> &'static str {
        match self {
            Self::WeChat => "wechat",
            Self::Lark => "lark",
        }
    }

    /// What a person calls it, in both the names they might look for.
    pub fn title(self) -> &'static str {
        match self {
            Self::WeChat => "WeChat / 微信",
            Self::Lark => "Lark / 飞书",
        }
    }

    /// What it costs to set one up, in one line, so the chooser is a decision
    /// rather than a pair of names.
    pub fn pitch(self) -> &'static str {
        match self {
            Self::WeChat => "scan a code · about ten seconds · a chat with just you",
            Self::Lark => "needs an app from open.feishu.cn · then pick a chat from a list",
        }
    }

    /// Whether a human's reply can find its way back to an agent this way.
    ///
    /// Both can, now. Written as a match rather than a `true` so that adding a
    /// kind that cannot forces the question to be answered: the panel must
    /// never let someone wait for a reply that is not coming.
    pub fn listens(self) -> bool {
        match self {
            Self::WeChat | Self::Lark => true,
        }
    }

    /// Whether there is exactly one human at the other end.
    ///
    /// It decides what an unaddressed sentence means. In a chat with one person
    /// in it, "yes, go ahead" plainly means whoever they were just talking to;
    /// in a group of eight it plainly does not, and guessing would put one
    /// person's aside in front of somebody else's agent.
    pub fn solo(self) -> bool {
        match self {
            Self::WeChat => true,
            Self::Lark => false,
        }
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
    /// Lark: the app id (`cli_…`). WeChat: the bot's own id, which exists only
    /// so a person with two of them can tell which is which.
    #[serde(default)]
    pub app_id: String,
    /// Lark: the app secret. WeChat: the bot token the scan handed back. The one
    /// field that must not appear anywhere a reader could be an agent.
    #[serde(default)]
    pub secret: String,
    /// Where a message lands. Lark: the chat id (`oc_…`). WeChat: the id of
    /// whoever scanned the code. Neither is typed by hand any more — both
    /// arrive from the platform when the binding is made.
    #[serde(default)]
    pub route: String,
    /// What to call that in a panel, since neither id means anything to read.
    #[serde(default)]
    pub route_label: String,
    /// WeChat: the host to use from here on, which the login may move to a
    /// nearer one than it was asked at. Empty for Lark, whose host never moves.
    #[serde(default)]
    pub base_url: String,
    /// WeChat: the token off the last thing the human said.
    ///
    /// It is the whole reason a ClawBot cannot speak first: a message out
    /// carries the token off the last message in, every inbound message brings
    /// a fresh one, and there is no way to invent one. Secret, because holding
    /// it is what lets anything post as this bot.
    #[serde(default)]
    pub context_token: String,
    /// Whether a message that names no channel goes here.
    #[serde(default)]
    pub preferred: bool,
}

impl ChannelBinding {
    /// The far end in words rather than ids: what the panel shows, and what a
    /// message says it went to.
    pub fn destination(&self) -> String {
        let named = self.route_label.trim();
        if !named.is_empty() {
            return named.to_string();
        }
        let route = self.route.trim();
        match self.kind {
            ChannelKind::WeChat if route.is_empty() => "not scanned yet".into(),
            ChannelKind::WeChat => "your WeChat".into(),
            ChannelKind::Lark if route.is_empty() => "no chat picked yet".into(),
            ChannelKind::Lark => route.to_string(),
        }
    }

    /// A one-line description for a panel or an error, with nothing secret in
    /// it.
    pub fn describes(&self) -> String {
        format!("{} · {}", self.kind.title(), self.destination())
    }

    /// What it takes to reach the platform at all.
    ///
    /// Which is not the same as what it takes to speak: a WeChat bot can be
    /// listened to from the moment it is scanned, and has to be, because
    /// listening is how it earns the right to answer.
    pub fn reachable(&self) -> Result<()> {
        let missing: &[&str] = match self.kind {
            ChannelKind::WeChat => &[
                needed(&self.secret, "bot token"),
                needed(&self.route, "person to talk to"),
            ],
            ChannelKind::Lark => &[
                needed(&self.app_id, "app id"),
                needed(&self.secret, "app secret"),
            ],
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
            "channel {} is missing its {}. Rebind it in the muxloom dashboard: \
             machines panel, press c.",
            self.id,
            missing.join(" and its ")
        )
    }

    /// Everything [`Self::reachable`] wants, and somewhere for a message to
    /// land.
    pub fn ready(&self) -> Result<()> {
        self.reachable()?;
        match self.kind {
            ChannelKind::WeChat if self.context_token.trim().is_empty() => bail!(
                "channel {} has not been spoken to yet. Say anything at all to the bot in \
                 WeChat — it can answer a conversation but it cannot start one.",
                self.id
            ),
            ChannelKind::Lark if self.route.trim().is_empty() => bail!(
                "channel {} has no chat to post into. Pick one in the muxloom dashboard: \
                 machines panel, press c.",
                self.id
            ),
            _ => Ok(()),
        }
    }

    /// The same binding with everything secret taken out, for anything that
    /// leaves this process other than a push to one of muxloom's own daemons.
    ///
    /// The context token goes with the app secret. It is short-lived and it
    /// looks like bookkeeping, but anything holding one can post as this bot,
    /// which is exactly what a secret is.
    pub fn redacted(&self) -> Self {
        Self {
            secret: String::new(),
            context_token: String::new(),
            ..self.clone()
        }
    }
}

/// The name of a field when it is blank, and nothing when it is filled in.
fn needed<'a>(value: &str, name: &'a str) -> &'a str {
    if value.trim().is_empty() { name } else { "" }
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
    ///
    /// Bindings are read one at a time, and one this build does not understand
    /// is dropped rather than taking the rest of the file with it. Two things
    /// this covers, both of which will happen: a kind that was removed between
    /// versions, and a kind added by a dashboard newer than the daemon reading
    /// this. Losing one channel until somebody rebinds it is a bad afternoon;
    /// losing the file is every channel on every machine at once.
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
        #[derive(Deserialize)]
        struct Loose {
            #[serde(default)]
            revision: u64,
            #[serde(default)]
            bindings: Vec<Value>,
        }
        let loose: Loose = serde_json::from_str(&text)
            .with_context(|| format!("invalid channels in {}", path.display()))?;
        let mut set = Self {
            revision: loose.revision,
            bindings: Vec::with_capacity(loose.bindings.len()),
        };
        for entry in loose.bindings {
            match serde_json::from_value::<ChannelBinding>(entry.clone()) {
                Ok(binding) => set.bindings.push(binding),
                Err(error) => debug::log(
                    "channel",
                    format!(
                        "skipped a binding this build does not understand ({error}); \
                         rebind it from the machines panel"
                    ),
                ),
            }
        }
        Ok(set)
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

/// Credentials found somewhere else on this machine, offered as a starting
/// point for a new binding.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Borrowed {
    /// The file they came out of, so the panel can say where the fields it
    /// filled in came from rather than appearing to know them.
    pub from: PathBuf,
    pub app_id: String,
    pub secret: String,
    pub route: String,
}

/// Read a Lark app out of a `cc-connect` configuration, if there is one.
///
/// Somebody who already talks to their agents through cc-connect has a working
/// app id and secret on disk; asking them to go back to the Lark console and
/// find both again is the kind of small tax that stops a feature from being
/// tried at all. Nothing is copied without being shown: these only ever land in
/// a form the person is looking at, and only when they open a new binding.
///
/// The file is walked rather than deserialized into a fixed shape. cc-connect's
/// own layout is not muxloom's to promise, and a key that moves into or out of
/// a table should not turn a convenience into a parse error.
pub fn borrow_cc_connect(path: &Path) -> Option<Borrowed> {
    let text = fs::read_to_string(path).ok()?;
    let table: toml::Value = text.parse().ok()?;
    let mut found = Borrowed {
        from: path.to_path_buf(),
        ..Borrowed::default()
    };
    fn walk(value: &toml::Value, found: &mut Borrowed) {
        let toml::Value::Table(table) = value else {
            return;
        };
        for (key, value) in table {
            if let toml::Value::String(text) = value {
                let slot = match key.as_str() {
                    "app_id" => &mut found.app_id,
                    "app_secret" | "secret" => &mut found.secret,
                    "chat_id" | "default_chat_id" => &mut found.route,
                    _ => continue,
                };
                if slot.is_empty() {
                    *slot = text.clone();
                }
            } else {
                walk(value, found);
            }
        }
    }
    walk(&table, &mut found);
    (!found.app_id.is_empty() && !found.secret.is_empty()).then_some(found)
}

/// Where cc-connect keeps the file above.
pub fn cc_connect_path() -> PathBuf {
    crate::config::expand_tilde("~/.cc-connect/config.toml")
}

/// Where Lark's open platform lives. `feishu.cn` and `larksuite.com` are the
/// same API under two names; a `cli_…` app id issued by 飞书 belongs to this one.
const LARK_HOST: &str = "https://open.feishu.cn";

/// The most text one message carries, in bytes. Both platforms answer something
/// larger by refusing it rather than by shortening it — so the shortening
/// happens here, where it can be said out loud in the message itself.
const LARK_LIMIT: usize = 20 * 1024;
const WECHAT_LIMIT: usize = 4 * 1024;

/// How long a message to a person may be, in characters.
///
/// Not a platform limit — [`LARK_LIMIT`] and [`WECHAT_LIMIT`] are those, in
/// bytes, because bytes are what the two APIs count. This one is about the
/// person: a card arrives on a phone and is read standing up, between other
/// things, and past a few paragraphs it stops being read at all. Characters
/// rather than bytes, so a message written in Chinese is not held to a third of
/// the length of one written in English.
pub const READABLE_LIMIT: usize = 1200;

/// How long the card's header may be. It is a few words saying what this is
/// about, and a header that needs a second line is a first line.
pub const TITLE_LIMIT: usize = 48;

/// Refuse an over-long message instead of trimming it.
///
/// Trimming is the tempting thing and the wrong one. An agent writing to a
/// person puts the conclusion at the top and the ask at the bottom, so a cut
/// takes the ask — the person is left with a card that stops mid-sentence and
/// no way to reach the rest, and the agent is never told it happened. Handing
/// it back is the only thing that gets a shorter message written, and a shorter
/// message is what was wanted. The platform limits below still clip, because
/// there a refusal would only lose a message the API would have carried.
pub fn refuse_if_too_long(message: &Outgoing) -> Result<()> {
    let title = message.title.trim().chars().count();
    if title > TITLE_LIMIT {
        bail!(
            "the title is {title} characters and the limit is {TITLE_LIMIT}. It is the card's \
             header — a few words saying what this is about, not a summary of it. Nothing was \
             sent: shorten the title and send again."
        );
    }
    let text = message.text.trim().chars().count();
    if text > READABLE_LIMIT {
        bail!(
            "the message is {text} characters and the limit is {READABLE_LIMIT}. This lands on \
             somebody's phone, so it has to be readable standing up: the conclusion, the numbers \
             that actually matter, and what you need from them. Everything else already exists \
             somewhere — the talk board, your session, the diff — so say where it is instead of \
             repeating it. Nothing was sent, and it was deliberately not cut down for you, \
             because what a cut removes is whatever you put last, which is usually the ask. \
             Rewrite it shorter and send again."
        );
    }
    Ok(())
}

/// How long before its stated expiry a tenant token is treated as spent. The
/// token is good for two hours; a minute covers a slow request and a clock that
/// is not quite right.
const TOKEN_MARGIN: Duration = Duration::from_secs(60);

/// A message on its way to a human.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Outgoing {
    /// What the message is about, in a few words. Lark shows it as the card's
    /// own header; WeChat, which has no header to show, gets it as the first
    /// line.
    pub title: String,
    /// The message, as markdown.
    pub text: String,
    /// One line naming who is speaking, put under the text. A human reading
    /// this on their phone has no other way to tell one agent from another.
    pub signature: String,
}

impl Outgoing {
    /// The title and the body as they go on the wire.
    ///
    /// The text itself is left almost exactly as it was written: the card
    /// renders GitHub-flavoured markdown, which is what an agent writes without
    /// being asked to, and rewriting someone's prose to protect a renderer
    /// costs more than it saves. Two things are done to it:
    ///
    /// - a leading `# ` line becomes the title when there is not one already,
    ///   because a card whose header and first line say the same thing reads as
    ///   a stutter, and
    /// - the whole thing is cut to fit, with the signature kept: a message that
    ///   was too long is still worth reading, and one the platform refused is
    ///   not a message at all.
    fn compose(&self, limit: usize) -> (String, String) {
        let mut title = self.title.trim().replace(['\r', '\n'], " ");
        let mut text = self.text.trim();
        if title.is_empty()
            && let Some(rest) = text.strip_prefix("# ")
        {
            let (heading, remainder) = rest.split_once('\n').unwrap_or((rest, ""));
            title = heading.trim().to_string();
            text = remainder.trim_start();
        }
        let signature = self.signature.trim();
        // The source goes at the very front, so whoever picks their phone up
        // sees who is talking before anything it said. It is a line of its
        // own, not part of the prose, and it stays even when the message is
        // cut to fit.
        let mut body = String::new();
        if !signature.is_empty() {
            body.push_str(signature);
            body.push_str("\n\n");
        }
        let body_limit = limit.saturating_sub(body.len());
        body.push_str(&clip(text, body_limit));
        (title, body)
    }
}

/// The text cut to fit on a character boundary, saying so where it was cut.
fn clip(text: &str, limit: usize) -> String {
    if text.len() <= limit {
        return text.to_string();
    }
    const MARK: &str = "\n\n*(cut to fit — the rest stayed on the machine)*";
    let mut end = limit.saturating_sub(MARK.len()).min(text.len());
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}{MARK}", &text[..end])
}

/// What a delivered message left behind.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Sent {
    /// The binding it went through, by the id an agent named.
    pub channel: String,
    /// That binding described without its secret, for the answer and the panel.
    pub through: String,
    /// The id a reply can be matched against: the platform's own, on both
    /// kinds. A human's reply — Lark's `parent_id`, WeChat's quote — names
    /// this id and nothing else, so a WeChat send that earned one keeps it.
    /// Only a send the body gave no id for (accepted but never delivered, so
    /// nothing can ever quote it) falls back to the id this side minted, and
    /// that fallback buys receipt bookkeeping, not reply matching.
    pub message_id: String,
    /// WeChat only: the platform's verdict on this send. None for Lark, since
    /// its HTTP status alone is sufficient to know whether delivery succeeded.
    pub wechat: Option<ilink::Verdict>,
}

/// Post one message to a human through one binding.
///
/// Whichever machine runs this is the machine that talks to the chat API, which
/// is why the credentials travel: an agent that finishes something at three in
/// the morning on a machine the controller is not watching still gets to say so.
pub fn send(
    binding: &ChannelBinding,
    message: &Outgoing,
    environment: &[(String, String)],
) -> Result<Sent> {
    send_reply(binding, message, None, environment)
}

/// Post one message that answers another by name, on the platforms that can.
///
/// `reply_to` is the platform's id for the message being answered — the very
/// id a prior send reported and a quote-reply arrived naming, which is the
/// same number seen from either end. WeChat draws it as a quote of that
/// message. Lark threads a reply by its own parent, which this send path does
/// not drive, so there `reply_to` changes nothing. This is the seam the
/// `send_channel_message` tool's `reply_to` argument comes through: whatever
/// an agent was told to answer, it answers quoting that.
pub fn send_reply(
    binding: &ChannelBinding,
    message: &Outgoing,
    reply_to: Option<&str>,
    environment: &[(String, String)],
) -> Result<Sent> {
    binding.ready()?;
    if message.text.trim().is_empty() {
        bail!("a channel message needs something to say");
    }
    match binding.kind {
        ChannelKind::Lark => send_lark(binding, message, environment),
        ChannelKind::WeChat => send_wechat(binding, message, reply_to, environment),
    }
}

/// Lark answers HTTP 200 with a non-zero `code` for everything it refuses, so
/// the status alone never says whether a message arrived. Checking this is what
/// keeps "sent" from meaning "the request was well formed".
fn lark_ok(answer: &Value, what: &str) -> Result<()> {
    match answer
        .get("code")
        .and_then(Value::as_i64)
        .unwrap_or_default()
    {
        0 => Ok(()),
        code => bail!(
            "Lark refused {what}: {} (code {code})",
            answer
                .get("msg")
                .and_then(Value::as_str)
                .unwrap_or("no reason given"),
        ),
    }
}

/// Tenant tokens already in hand, by app id.
///
/// Lark rate-limits the token endpoint harder than the message one, and the
/// controller asks for a token every few seconds once it is watching a chat.
/// Keyed by app id rather than by binding so two chats behind one app share it.
fn token_cache() -> &'static Mutex<HashMap<String, (String, Instant)>> {
    static CACHE: OnceLock<Mutex<HashMap<String, (String, Instant)>>> = OnceLock::new();
    CACHE.get_or_init(Default::default)
}

fn tenant_token(binding: &ChannelBinding, environment: &[(String, String)]) -> Result<String> {
    let app_id = binding.app_id.trim().to_string();
    let held = {
        let cache = token_cache()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        cache
            .get(&app_id)
            .filter(|(_, until)| Instant::now() < *until)
            .map(|(token, _)| token.clone())
    };
    if let Some(token) = held {
        return Ok(token);
    }
    let answer = http::post_json(
        &format!("{LARK_HOST}/open-apis/auth/v3/tenant_access_token/internal"),
        &[],
        &json!({ "app_id": app_id, "app_secret": binding.secret.trim() }),
        environment,
    )
    .with_context(|| format!("channel {} could not reach Lark", binding.id))?;
    lark_ok(&answer, "these credentials")?;
    let token = answer
        .get("tenant_access_token")
        .and_then(Value::as_str)
        .context("Lark accepted the credentials but issued no token")?
        .to_string();
    let lifetime =
        Duration::from_secs(answer.get("expire").and_then(Value::as_u64).unwrap_or(7200));
    token_cache()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .insert(
            app_id,
            (
                token.clone(),
                Instant::now() + lifetime.saturating_sub(TOKEN_MARGIN),
            ),
        );
    Ok(token)
}

/// The card one message becomes.
///
/// Card JSON 2.0, and it has to be: the 1.0 markdown component renders bold,
/// italics and links and nothing else, so headings, tables, block quotes and
/// inline code — most of what makes a summary readable on a phone — arrive as
/// the literal characters someone typed. 2.0 renders GitHub-flavoured markdown.
/// It wants a Lark client from 7.20 on; an older one shows the title and an
/// upgrade notice where the body would be.
///
/// Deliberately minimal past that. Every field 2.0 allows is another thing the
/// platform can reject, and a rejected card is a message the human never sees.
fn lark_card(title: &str, body: &str) -> Value {
    let mut card = json!({
        "schema": "2.0",
        "body": { "elements": [{ "tag": "markdown", "content": body }] },
    });
    if !title.is_empty() {
        card["header"] = json!({
            "title": { "tag": "plain_text", "content": title },
            "template": "blue",
        });
    }
    card
}

fn send_lark(
    binding: &ChannelBinding,
    message: &Outgoing,
    environment: &[(String, String)],
) -> Result<Sent> {
    let (title, body) = message.compose(LARK_LIMIT);
    let authorization = format!("Bearer {}", tenant_token(binding, environment)?);
    let answer = http::post_json(
        &format!("{LARK_HOST}/open-apis/im/v1/messages?receive_id_type=chat_id"),
        &[("Authorization", authorization.as_str())],
        &json!({
            "receive_id": binding.route.trim(),
            "msg_type": "interactive",
            "content": serde_json::to_string(&lark_card(&title, &body))?,
        }),
        environment,
    )?;
    lark_ok(&answer, "the message")?;
    Ok(Sent {
        channel: binding.id.clone(),
        through: binding.describes(),
        message_id: answer
            .pointer("/data/message_id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        wechat: None,
    })
}

/// A reaction a human reads as "the agent has seen this". Deliberately
/// friendly: the point is to say someone is looking, not to fill the chat
/// with a formal sticker.
const LARK_RECEIPT_EMOJI: &str = "FINGERHEART";

/// Put a small reaction on a message the human just sent, so they know the
/// agent has seen it without the bot having to say a word. Lark can mark a
/// message in place; this is that mark, and it is exactly what `☕️ 收到` a
/// WeChat receipt would do, without adding a second message to the chat.
fn ack_lark(
    binding: &ChannelBinding,
    message_id: &str,
    environment: &[(String, String)],
) -> Result<()> {
    if message_id.trim().is_empty() {
        return Ok(());
    }
    let authorization = format!("Bearer {}", tenant_token(binding, environment)?);
    let answer = http::post_json(
        &format!(
            "{LARK_HOST}/open-apis/im/v1/messages/{}/reactions",
            message_id.trim()
        ),
        &[("Authorization", authorization.as_str())],
        &json!({
            "reaction_type": { "emoji_type": LARK_RECEIPT_EMOJI },
        }),
        environment,
    )?;
    lark_ok(&answer, "a receipt reaction").context("Lark would not mark the message as received")
}

/// Where a Lark app is made, and the one page both of its strings are copied
/// out of. Written down rather than described: somebody who has to find it by
/// searching has already spent longer than the whole of the rest of this takes.
pub const LARK_CONSOLE: &str = "https://open.feishu.cn/app";

/// The link that opens a chat with one app's bot in the Lark client.
///
/// An AppLink rather than a page: `applink.feishu.cn` addresses are handled by
/// the client itself, so a phone that scans this lands on the bot instead of in
/// a browser. It takes the app id and nothing else, and wants a client from
/// 3.40 on; an older one, or an account the app was never released to, is shown
/// Lark's own explanation rather than a blank screen.
///
/// The chat it opens is the one-to-one one. That is the point: once the person
/// has said anything to the bot, the conversation is a chat `oc_…` the same
/// [`chats`] list answers with, flagged `chat_mode` `p2p`, and it binds like
/// any other. Groups still work — adding the bot to one is a tap — but a
/// private message is the shortest way to a working channel.
///
/// The id is trimmed and otherwise used as it stands. Nothing reaches here until
/// Lark has issued a token for that very id, so an id that would need escaping
/// is an id that never gets this far.
pub fn lark_bot_link(app_id: &str) -> String {
    format!(
        "https://applink.feishu.cn/client/bot/open?appId={}",
        app_id.trim()
    )
}

/// One chat a Lark app's bot can talk in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chat {
    pub id: String,
    pub name: String,
    /// How the two halves of a chat differ in Lark: `p2p` is the one-to-one
    /// conversation with the bot, `group` is a room it has been added to.
    pub chat_mode: String,
}

impl Chat {
    /// The row the chooser shows: a direct message and a room are named the
    /// same way, so they need telling apart.
    pub fn label(&self) -> String {
        match self.chat_mode.as_str() {
            "p2p" => format!("💬 {}", self.name),
            _ => format!("👥 {}", self.name),
        }
    }
}

/// The chats a Lark app can already talk in, newest first.
///
/// A chat id is `oc_` and thirty-odd characters of hexadecimal, and there is
/// nowhere in the Lark client to copy one from — the way people find theirs is
/// to open a chat in a browser and read the address bar. Asking is better: the
/// app knows which chats it is in, and the answer comes back with the names
/// they are known by.
///
/// Groups and one-to-one conversations both come back here: each item carries a
/// `chat_mode` (`group` or `p2p`). A one-to-one conversation appears as soon as
/// the person has said anything to the bot, which is what makes a private
/// message a first-class way to reach a channel — no group required.
///
/// One page. A hundred chats is far more than a bot bound to a dashboard is in,
/// and paging a chooser nobody will scroll is complexity bought for nobody.
pub fn chats(app_id: &str, secret: &str, environment: &[(String, String)]) -> Result<Vec<Chat>> {
    // Borrows the token cache and the refusal check by borrowing the shape
    // those want, so the two credentials being tried out here are checked
    // exactly the way they will be when a message goes through them.
    let asking = ChannelBinding {
        id: "the app".into(),
        kind: ChannelKind::Lark,
        app_id: app_id.trim().into(),
        secret: secret.trim().into(),
        ..Default::default()
    };
    let authorization = format!("Bearer {}", tenant_token(&asking, environment)?);
    let answer = http::get_json(
        &format!("{LARK_HOST}/open-apis/im/v1/chats?page_size=100&sort_type=ByActiveTimeDesc"),
        &[("Authorization", authorization.as_str())],
        environment,
    )
    .context("could not ask Lark which chats this app is in")?;
    lark_ok(&answer, "that request")?;
    Ok(answer
        .pointer("/data/items")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default()
        .iter()
        .filter_map(|item| {
            let id = item.get("chat_id").and_then(Value::as_str)?;
            let name = item
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .trim();
            let chat_mode = item
                .get("chat_mode")
                .and_then(Value::as_str)
                .unwrap_or("group")
                .to_string();
            Some(Chat {
                id: id.to_string(),
                // A group with no name set is not an error and should not read
                // as an empty row; Lark itself shows these by their members.
                // A direct message the API leaves unnamed is the person on the
                // other end of it, so it is named for that instead.
                name: match (name.is_empty(), chat_mode.as_str()) {
                    (true, "p2p") => "(direct message)".to_string(),
                    (true, _) => "(unnamed chat)".to_string(),
                    (false, _) => name.to_string(),
                },
                chat_mode,
            })
        })
        .collect())
}

/// The bot behind one WeChat binding, in the terms the protocol wants.
fn wechat_account(binding: &ChannelBinding) -> ilink::Account {
    ilink::Account {
        bot_id: binding.app_id.trim().to_string(),
        token: binding.secret.trim().to_string(),
        base_url: Some(binding.base_url.trim().to_string())
            .filter(|url| !url.is_empty())
            .unwrap_or_else(|| ilink::HOST.to_string()),
        user_id: binding.route.trim().to_string(),
    }
}

/// Markdown flattened into something worth reading in a chat that renders none
/// of it.
///
/// WeChat shows a bot's message as plain text, exactly as it arrives. An agent
/// writes markdown without being asked to, so leaving it alone means somebody
/// reading `**done**` and a row of pipes on their phone. Only the marks are
/// taken off — the words, the line breaks and the order are left exactly as
/// they were written, because guessing at someone's prose is how a summary
/// stops being one.
fn plain(markdown: &str) -> String {
    let mut out = Vec::new();
    let mut fenced = false;
    for line in markdown.lines() {
        let trimmed = line.trim_end();
        if trimmed.trim_start().starts_with("```") {
            // A fence is a mark with nothing in it; the code between two of
            // them is the point, and stays.
            fenced = !fenced;
            continue;
        }
        if fenced {
            out.push(trimmed.to_string());
            continue;
        }
        let body = trimmed.trim_start();
        let indent = &trimmed[..trimmed.len() - body.len()];
        let body = match body.strip_prefix('#') {
            // A heading is a line that matters, so it keeps its emphasis in the
            // only way plain text has: on its own, with a blank line over it.
            Some(_) => {
                let heading = body.trim_start_matches('#').trim();
                if out.last().is_some_and(|last: &String| !last.is_empty()) {
                    out.push(String::new());
                }
                heading.to_string()
            }
            None => match body
                .strip_prefix("- ")
                .or_else(|| body.strip_prefix("* "))
                .or_else(|| body.strip_prefix("+ "))
            {
                Some(item) => format!("· {item}"),
                None => body.to_string(),
            },
        };
        out.push(format!("{indent}{}", inline(&body)));
    }
    // Blank lines top and bottom go; the indentation on the first line stays.
    // Trimming the joined string would take both, and the first line of an
    // agent's message is quite often the first line of a code block.
    let blank = |line: &String| line.trim().is_empty();
    let from = out
        .iter()
        .position(|line| !blank(line))
        .unwrap_or(out.len());
    let upto = out
        .iter()
        .rposition(|line| !blank(line))
        .map_or(from, |at| at + 1);
    out[from..upto].join("\n")
}

/// The marks that sit inside a line: emphasis, code ticks, and links.
fn inline(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut rest = line;
    while let Some(open) = rest.find('[') {
        // `[what it says](where it goes)` keeps both halves — the words on
        // their own would lose the address, and a bare URL is unreadable.
        let Some(shut) = rest[open..].find("](").map(|at| open + at) else {
            break;
        };
        let Some(end) = rest[shut + 2..].find(')').map(|at| shut + 2 + at) else {
            break;
        };
        out.push_str(&rest[..open]);
        let (words, url) = (&rest[open + 1..shut], &rest[shut + 2..end]);
        out.push_str(&if words.is_empty() {
            url.to_string()
        } else {
            format!("{words} ({url})")
        });
        rest = &rest[end + 1..];
    }
    out.push_str(rest);
    out.replace("**", "")
        .replace("__", "")
        .replace('`', "")
        .trim_end()
        .to_string()
}

fn send_wechat(
    binding: &ChannelBinding,
    message: &Outgoing,
    reply_to: Option<&str>,
    environment: &[(String, String)],
) -> Result<Sent> {
    let (title, body) = message.compose(WECHAT_LIMIT);
    // A WeChat message has no header of its own, so a title goes back into the
    // text it was lifted out of, on a line by itself.
    let text = match title.is_empty() {
        true => plain(&body),
        false => format!("{title}\n\n{}", plain(&body)),
    };
    let (client_id, verdict) = ilink::send_text(
        &wechat_account(binding),
        binding.context_token.trim(),
        &text,
        reply_to,
        environment,
    )
    .with_context(|| format!("channel {} could not reach WeChat", binding.id))?;
    // What a quote of this message will name is WeChat's own id from the
    // reply body, not the `client_id` this side minted — so the id the tool
    // result hands the agent and the receipt keeps is the platform's whenever
    // it issued one. A send the body gave no id for was never delivered and
    // can never be quoted; the minted id is kept for bookkeeping only.
    let message_id = verdict.message_id.clone().unwrap_or(client_id);
    Ok(Sent {
        channel: binding.id.clone(),
        through: binding.describes(),
        message_id,
        wechat: Some(verdict),
    })
}

/// One session, anywhere in the fleet, as the human's end of a conversation
/// knows it.
///
/// The machine is the id the dashboard uses, not the origin key a session
/// carries in its environment: a receipt is stamped with it when the dashboard
/// collects it from that machine, which is the one moment both names are in the
/// same place.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Correspondent {
    #[serde(default)]
    pub machine: String,
    pub session_id: String,
    /// What to call it in a chat message, where a session id means nothing.
    #[serde(default)]
    pub label: String,
    /// The working directory the session runs in, when the account knows it.
    /// Kept so `/list` can group agents by folder.
    #[serde(default)]
    pub path: String,
    /// Whether the session is alive and answering. A session with no pid is
    /// finished, whatever the account says; absent from an old record is
    /// treated as active so a hand-written aim still reaches it.
    #[serde(default = "default_true")]
    pub alive: bool,
    /// Whether the session is engaged right now, not just alive. Shown as the
    /// working marker in `/list`; absent from an old record is treated as
    /// idle rather than guessing.
    #[serde(default)]
    pub working: bool,
    /// Whether the session has stopped on a question - an approval prompt, a
    /// menu - and is waiting for a person. Shown in `/list` too: an agent
    /// nobody answers waits for ever, and from a phone that reads exactly
    /// like an agent that is working.
    #[serde(default)]
    pub needs_attention: bool,
    /// A glance at what the session has been doing, for `/list`. This is the
    /// daemon's `recap` — the last thing the model said.
    #[serde(default)]
    pub recap: Option<String>,
    /// The agent that started this one, when an agent did. `/list` leaves those
    /// out: work an agent hands to a subagent is still that agent's to answer
    /// for, and a phone showing every one of them shows a list nobody can find
    /// their own agent in. Absent from an old record, which is what a session
    /// nobody started looks like too, so both read as a main agent.
    #[serde(default)]
    pub parent: Option<String>,
}

fn default_true() -> bool {
    true
}

impl Correspondent {
    /// How the human reads it, machine and all.
    ///
    /// Never the session id. `muxloomd-claude-1787996682-39374-2` identifies
    /// the session exactly and names it not at all, and a person reading their
    /// phone can do nothing with it but squint — what it has been doing, or
    /// failing that its number, at least says which agent this is.
    pub fn name(&self) -> String {
        let called = self.list_name();
        if self.machine.is_empty() {
            return called;
        }
        format!("{called} · {}", self.machine)
    }

    /// The same session without the machine qualifier — for a list that is
    /// already grouped under a machine, where repeating the machine on every
    /// row is noise.
    fn list_name(&self) -> String {
        if !self.label.trim().is_empty() {
            return self.label.trim().to_string();
        }
        if let Some(recap) = self.recap.as_ref() {
            let first = recap.split('\n').next().unwrap_or("").trim();
            if !first.is_empty() {
                return if first.chars().count() > 60 {
                    let cut: String = first.chars().take(57).collect();
                    format!("{cut}…")
                } else {
                    first.to_string()
                };
            }
        }
        self.session_id
            .rsplit('-')
            .next()
            .unwrap_or(&self.session_id)
            .to_string()
    }

    /// What the agent is doing, in the words a person reading their phone can
    /// act on. There is no colour in a chat message and no spinner: without
    /// this, the only thing under an agent's name is its recap, and a recap
    /// that reads like live activity is what an agent looks like whether it
    /// is mid-turn, stopped on a question nobody answered, or finished hours
    /// ago.
    fn state(&self) -> &'static str {
        if !self.alive {
            return "finished";
        }
        if self.needs_attention {
            return "waiting for you";
        }
        match self.working {
            true => "working",
            false => "idle",
        }
    }

    /// Whether a `/select` word picks this session out.
    fn answers_to(&self, needle: &str) -> bool {
        let needle = needle.to_lowercase();
        let (machine, session) = needle
            .split_once('/')
            .map(|(machine, session)| (machine.trim(), session.trim()))
            .unwrap_or(("", needle.trim()));
        if !machine.is_empty() && !self.machine.to_lowercase().starts_with(machine) {
            return false;
        }
        session.is_empty()
            || self.session_id.to_lowercase().starts_with(session)
            || self.label.to_lowercase().contains(session)
    }
}

/// One message muxloom put in front of a human, kept so their reply to it can
/// find the agent that wrote it.
///
/// It carries no secret and no text — only which chat message this was and who
/// is owed the answer — so it is safe to hand between machines on the same
/// round that carries everything else.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChannelReceipt {
    /// The binding it went out through.
    #[serde(default)]
    pub channel: String,
    /// The platform's id for the message, which is what a reply names.
    pub message_id: String,
    /// The machine the answering agent runs on, so a quote that lands on a
    /// different dashboard can still name where its agent lives.
    #[serde(default)]
    pub machine: String,
    #[serde(default)]
    pub session_id: String,
    #[serde(default)]
    pub label: String,
}

/// The most receipts a machine holds for a dashboard that has stopped
/// collecting them. Old ones are the ones nobody is going to reply to.
pub const RECEIPT_CAP: usize = 256;

/// Everything reading a chat needs to remember between rounds.
///
/// It lives on the dashboard, because the dashboard is the only thing that
/// reads: one reader means a message is routed once, with no agreement to
/// reach about who saw it first.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Inbox {
    /// Per binding: the newest message time already looked at, epoch
    /// milliseconds. The next round asks for everything from a second before
    /// it and leans on `handled` to not say anything twice.
    #[serde(default)]
    pub seen: HashMap<String, u64>,
    /// Per WeChat binding: the server's own bookkeeping for where it had got
    /// to, kept exactly as it came. It is not ours to read — only to hand back.
    #[serde(default)]
    pub cursors: HashMap<String, String>,
    /// Per WeChat binding: when it is worth asking again, epoch milliseconds.
    /// Set when WeChat says the conversation has gone to sleep, which is a
    /// thing only the person can undo.
    #[serde(default)]
    pub waking: HashMap<String, u64>,
    /// Per binding: who a plain message goes to, until `/select` says otherwise.
    #[serde(default)]
    pub aimed: HashMap<String, Correspondent>,
    /// Per binding: the agent kind `/call` and `/new` should start. Set with
    /// `/agent <kind>`; absent means the default (claude). Kept so a person
    /// does not re-type their preference on every call.
    #[serde(default)]
    pub default_kind: HashMap<String, AgentKind>,
    /// Per binding: which agent kind the last `/call` used, so a bare `/call`
    /// keeps the kind it last ran without the person re-selecting.
    #[serde(default)]
    pub last_call_kind: HashMap<String, AgentKind>,
    /// Which agent sent each message a human might reply to, newest last.
    #[serde(default)]
    pub receipts: Vec<ChannelReceipt>,
    /// Message ids already routed, newest last: reading the same window twice
    /// must not deliver twice.
    #[serde(default)]
    pub handled: Vec<String>,
    /// Per WeChat binding: when this machine last put its lease on the board,
    /// epoch milliseconds. Zero means it never has, which is how a fresh
    /// dashboard knows it still has to introduce itself to the fleet.
    #[serde(default)]
    pub lease_claims: HashMap<String, u64>,
    /// Per binding: how many replies this machine has sent into it. Two
    /// machines bound to one chat must look different to the person reading
    /// their phone, and the number is the part only true on the sender.
    #[serde(default)]
    pub reply_counts: HashMap<String, u64>,
}

impl Inbox {
    /// Read what is remembered, treating anything unreadable as nothing
    /// remembered: the cost is a chat that repeats itself once, which is much
    /// cheaper than a dashboard that will not start.
    pub fn load(path: &Path) -> Self {
        fs::read_to_string(path)
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        let temporary = path.with_extension("json.tmp");
        fs::write(
            &temporary,
            format!("{}\n", serde_json::to_string_pretty(self)?),
        )
        .with_context(|| format!("failed to write {}", temporary.display()))?;
        fs::rename(&temporary, path)
            .with_context(|| format!("failed to replace {}", path.display()))
    }

    /// Take one receipt in, dropping the oldest once the list is full.
    pub fn remember(&mut self, receipt: ChannelReceipt) {
        self.receipts
            .retain(|held| held.message_id != receipt.message_id);
        self.receipts.push(receipt);
        let over = self.receipts.len().saturating_sub(RECEIPT_CAP);
        self.receipts.drain(..over);
    }

    /// Point this chat at somebody, from here on.
    ///
    /// Aiming also files a receipt in that agent's name, because an aim that is
    /// not also the last thing said would be beaten by whatever card happened
    /// to arrive before it: an unaddressed sentence answers the last sender
    /// first. The id is muxloom's own and names no message on the far end, so
    /// nobody can quote it — it exists to put the newly aimed agent at the end
    /// of the line, which is where `/select` means to put it.
    pub fn aim(&mut self, binding: &str, who: Correspondent) {
        self.remember(ChannelReceipt {
            channel: binding.to_string(),
            message_id: aim_receipt_id(binding),
            machine: who.machine.clone(),
            session_id: who.session_id.clone(),
            label: who.label.clone(),
        });
        self.aimed.insert(binding.to_string(), who);
    }

    /// Stop pointing this chat anywhere, taking back the receipt aiming filed:
    /// an aim nobody wants any more must not go on answering as the last
    /// sender either.
    pub fn unaim(&mut self, binding: &str) -> Option<Correspondent> {
        let id = aim_receipt_id(binding);
        self.receipts.retain(|receipt| receipt.message_id != id);
        self.aimed.remove(binding)
    }

    /// Forget a session that is not there any more: its aim, and every receipt
    /// naming it. Both have to go — a chat whose last sender has exited would
    /// otherwise answer that same dead session forever, once per sentence.
    pub fn forget(&mut self, binding: &str, session_id: &str) {
        if session_id.is_empty() {
            return;
        }
        self.receipts
            .retain(|receipt| receipt.session_id != session_id);
        if self
            .aimed
            .get(binding)
            .is_some_and(|aim| aim.session_id == session_id)
        {
            self.aimed.remove(binding);
        }
    }

    fn mark(&mut self, message_id: &str) {
        self.handled.push(message_id.to_string());
        let over = self.handled.len().saturating_sub(RECEIPT_CAP * 2);
        self.handled.drain(..over);
    }

    fn sender_of(&self, message_id: &str) -> Option<Correspondent> {
        self.receipts
            .iter()
            .find(|receipt| receipt.message_id == message_id)
            .map(Self::correspondent)
    }

    /// Whoever spoke through this binding most recently.
    ///
    /// Only consulted for a chat with one person in it, where an answer with no
    /// address on it can only sensibly mean the last thing said.
    fn last_sender(&self, channel: &str) -> Option<Correspondent> {
        self.receipts
            .iter()
            .rev()
            .find(|receipt| receipt.channel == channel && !receipt.session_id.is_empty())
            .map(Self::correspondent)
    }

    /// A receipt read as somebody to talk to. The machine is left blank: a
    /// receipt only ever remembered a session, and which machine it is on is
    /// settled later by whoever can ask.
    fn correspondent(receipt: &ChannelReceipt) -> Correspondent {
        Correspondent {
            machine: receipt.machine.clone(),
            session_id: receipt.session_id.clone(),
            label: receipt.label.clone(),
            // A recipient recovered from a receipt is assumed live for the
            // purpose of answering it: the receipt is what a live agent just
            // sent, so the most recent word is that it was here.
            ..Default::default()
        }
    }

    /// The line the next reply out this binding carries: which machine sent it,
    /// and which of this machine's replies it is. With two machines bound to
    /// the same chat the text alone cannot tell the voices apart, so the
    /// counter makes identical replies from one machine still readable as a
    /// sequence rather than as a stutter. Empty machine name means the identity
    /// is unknown, and the reply goes out unsigned as before.
    pub fn reply_signature(&mut self, binding: &str, machine: &str) -> String {
        if machine.trim().is_empty() {
            return String::new();
        }
        let count = self.reply_counts.entry(binding.to_string()).or_insert(0);
        *count = count.saturating_add(1);
        format!("*— {machine} #{count}*")
    }
}

/// The id muxloom files its own aim receipt under, one per binding. Deliberately
/// not shaped like a platform message id: nothing on the far end can name it.
fn aim_receipt_id(binding: &str) -> String {
    format!("muxloom-aim:{binding}")
}

/// Where a dashboard keeps what it has read, given its state directory. Beside
/// the channels, but a separate file: this one holds no secret and is rewritten
/// every few seconds.
pub fn inbox_path_in(state_dir: &Path) -> PathBuf {
    state_dir.join("channel-inbox.json")
}

/// What is to be done with one thing a human said.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Route {
    /// Hand it to one agent.
    Agent(Correspondent),
    /// Point this chat at whoever matches these words, from here on.
    Aim(String),
    /// List the machines, or (with a machine number) the agents on one of
    /// them. `arg` carries the part after `/list`.
    List(String),
    /// Start a new agent. `arg` carries what came after `/new` — a folder, if
    /// one was given.
    New(String),
    /// Run what comes after `/call` with a fresh one-shot agent of the kind
    /// the last `/call` used (default claude), in a temporary scratch folder.
    Call(String),
    /// Set the default agent kind `/call` and `/new` start, for this chat.
    AgentKind(String),
    /// Say what this chat is aimed at and what kind it will start.
    Current,
    /// Stop the agent this chat is aimed at.
    Stop,
    /// A person answering a cross-machine approval ask. `id` is the approval
    /// id (`approve-N`), and `verdict` whether they said one-shot yes, always,
    /// or no.
    Approval {
        id: String,
        verdict: crate::approvals::Verdict,
    },
    /// Say what there is to talk to.
    Who,
    /// Explain the commands this chat understands.
    Help,
    /// Stop pointing anywhere; plain messages go back to the board.
    Clear,
    /// Put it on the machine board, where any agent can find it. `asked` is
    /// whether the human meant to — `/all` — or whether it simply had nowhere
    /// better to go. Nothing a person says is dropped.
    Board { asked: bool },
    /// A word that was meant as a command and is not one. Answered rather than
    /// sent: somebody who mistypes `/select` and has it delivered to an agent
    /// as the literal word believes they aimed this chat, and every sentence
    /// after it goes somewhere they did not intend.
    Unknown(String),
}

/// Where one incoming message goes, and what is left of it once the command
/// word is off the front.
///
/// Written as a function of what is remembered rather than of what is
/// reachable: which sessions exist is settled afterwards, by whoever can ask.
///
/// `solo` says whether there is one person at the far end. In a chat with one
/// person in it, a sentence with no address on it can only mean whoever they
/// were last talking to, and making them say `/select` first to hear that back
/// is the kind of ceremony that gets a feature abandoned. In a group it means
/// nothing of the sort, so there it goes to the board where anyone can find it.
pub fn route(
    text: &str,
    reply_to: Option<&str>,
    binding: &str,
    solo: bool,
    inbox: &Inbox,
) -> (Route, String) {
    let text = text.trim();
    // A bare approval reply (`approve-12`, `always-12`, `reject-12`) answers
    // a cross-machine write the controller parked and asked about. It is not a
    // slash command; intercept it whole so it never reaches a session as prose.
    if let Some(route) = approval_route(text) {
        return (route, String::new());
    }
    let (word, rest) = text.split_once(char::is_whitespace).unwrap_or((text, ""));
    let rest = rest.trim().to_string();
    // A command is a command wherever it was typed, including in a reply: the
    // person who writes `/who` under a card wants the list, not to send the
    // word `/who` to an agent.
    match word.to_lowercase().as_str() {
        "/select" | "/s" => return (Route::Aim(rest.clone()), rest),
        "/list" | "/l" => return (Route::List(rest.clone()), rest),
        "/call" => return (Route::Call(rest.clone()), rest),
        "/agent" | "/model" => return (Route::AgentKind(rest.clone()), rest),
        "/current" => return (Route::Current, rest),
        "/stop" => return (Route::Stop, rest),
        "/new" => return (Route::New(rest.clone()), rest),
        "/who" => return (Route::Who, rest),
        "/help" | "/h" | "/?" => return (Route::Help, rest),
        "/clear" => return (Route::Clear, rest),
        "/all" => return (Route::Board { asked: true }, rest),
        other if meant_as_a_command(other) => {
            return (Route::Unknown(other.to_string()), rest);
        }
        _ => {}
    }
    // Replying to a card is the shortest way to answer the agent that wrote it,
    // and the one that needs nothing learnt in advance.
    if let Some(who) = reply_to.and_then(|id| inbox.sender_of(id)) {
        return (Route::Agent(who), text.to_string());
    }
    // Then whoever spoke last. A person reading their phone answers the card
    // in front of them; if that is not the agent this chat was aimed at an hour
    // ago, the card is what they meant and the aim is stale. So the aim is a
    // fallback rather than a lock — and aiming files a receipt of its own
    // (`Inbox::aim`), which is what makes a fresh `/select` the last word too.
    if solo && let Some(who) = inbox.last_sender(binding) {
        return (Route::Agent(who), text.to_string());
    }
    if let Some(who) = inbox.aimed.get(binding) {
        return (Route::Agent(who.clone()), text.to_string());
    }
    (Route::Board { asked: false }, text.to_string())
}

/// Whether a word that is not one of the commands was nonetheless typed as
/// one.
///
/// A slash and letters, and nothing else: `/slect` is a mistyped command,
/// `/tmp/x` and `/etc/hosts` and `/usr/bin/env` are paths somebody is talking
/// about. A bare `/tmp` at the very start of a message is the one case this
/// reads wrongly, and the answer it gets says how to send it anyway — which is
/// a far smaller price than a silently misdelivered `/slect`.
/// Resolve a `/agent` switch word to an agent kind, case-insensitively.
pub fn parse_kind(word: &str) -> Option<AgentKind> {
    match word.trim().to_ascii_lowercase().as_str() {
        "claude" => Some(AgentKind::Claude),
        "pi" => Some(AgentKind::Pi),
        "codex" => Some(AgentKind::Codex),
        "opencode" => Some(AgentKind::OpenCode),
        _ => None,
    }
}

/// Recognise a bare approval reply — `approve-12`, `always-12`, `reject-12` —
/// and turn it into an Approval route. Anything that is not one of those three
/// words followed by a numeric id is left alone.
pub fn approval_route(text: &str) -> Option<Route> {
    let (word, id) = text.trim().split_once('-')?;
    if id.is_empty() || !id.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let verdict = match word.trim().to_ascii_lowercase().as_str() {
        "approve" => crate::approvals::Verdict::Yes,
        "always" => crate::approvals::Verdict::Always,
        "reject" => crate::approvals::Verdict::No,
        _ => return None,
    };
    Some(Route::Approval {
        id: format!("approve-{id}"),
        verdict,
    })
}

fn meant_as_a_command(word: &str) -> bool {
    let Some(name) = word.strip_prefix('/') else {
        return false;
    };
    !name.is_empty()
        && name
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'))
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
    /// What agents out there told the human while the dashboard was not
    /// looking, collected on the way past so their replies can find them.
    pub receipts: Vec<ChannelReceipt>,
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
            Ok(None) => {
                // It answered — it is reachable, even without channels.
                heard_from(target);
                continue;
            }
            Ok(Some((held, receipts))) => {
                heard_from(target);
                round.asked += 1;
                round.receipts.extend(receipts);
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
                no_longer_heard(target);
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

/// One thing a human said, with only the parts routing depends on.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Incoming {
    pub message_id: String,
    /// The message it answers, when it answers one. Lark carries it as the
    /// message's `parent_id`; WeChat delivers a quote-reply as an ordinary
    /// message whose item names the quoted message by the very id that send's
    /// reply body handed back — so on both kinds this matches a receipt, and
    /// a quoted answer finds the agent that wrote the quoted message.
    pub reply_to: Option<String>,
    /// Epoch milliseconds, as the platform recorded it.
    pub at: u64,
    pub text: String,
    /// WeChat: the token that has to travel on anything said back. Empty for
    /// Lark, which needs no such thing.
    pub context_token: String,
    /// Said before anything here was listening, so it is read for what it
    /// carries and not delivered to anybody.
    ///
    /// It is not the same as having been handled: nothing was done about it and
    /// nothing will be. What it is still good for is the token above, which is
    /// the only thing that ever lets a WeChat bot speak.
    pub stale: bool,
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since| since.as_millis() as u64)
        .unwrap_or_default()
}

/// The readable part of one Lark message. `body.content` is a JSON document
/// inside a string, and which document depends on the type: plain text is one
/// field, a rich post is paragraphs of runs. Anything else — an image, a file,
/// a card — has no text to route, and saying nothing about it is better than
/// waking an agent with an empty message.
fn lark_text(item: &Value) -> Option<String> {
    let content: Value = serde_json::from_str(item.pointer("/body/content")?.as_str()?).ok()?;
    let text = match item.get("msg_type").and_then(Value::as_str)? {
        "text" => content.get("text")?.as_str()?.to_string(),
        "post" => content
            .get("content")?
            .as_array()?
            .iter()
            .filter_map(Value::as_array)
            .map(|line| {
                line.iter()
                    .filter_map(|run| run.get("text").and_then(Value::as_str))
                    .collect::<Vec<_>>()
                    .join("")
            })
            .collect::<Vec<_>>()
            .join("\n"),
        _ => return None,
    };
    // `@`-ing the bot is how a group message reaches it at all, and it arrives
    // as a placeholder the human never typed. Taking it out leaves what they
    // meant to say.
    let text = text
        .split_whitespace()
        .filter(|word| !word.starts_with("@_user_") && *word != "@_all")
        .collect::<Vec<_>>()
        .join(" ");
    Some(text).filter(|text| !text.trim().is_empty())
}

/// Everything said in one chat since `since`, oldest first.
///
/// Asked for from a second early and deduplicated by id afterwards, because the
/// window is in seconds and the cursor is in milliseconds: overlapping is free,
/// and a message that falls between two rounds is gone for good.
fn lark_inbox(
    binding: &ChannelBinding,
    since: u64,
    environment: &[(String, String)],
) -> Result<Vec<Incoming>> {
    let authorization = format!("Bearer {}", tenant_token(binding, environment)?);
    let answer = http::get_json(
        &format!(
            "{LARK_HOST}/open-apis/im/v1/messages?container_id_type=chat&container_id={}\
             &sort_type=ByCreateTimeAsc&page_size=50&start_time={}",
            binding.route.trim(),
            (since / 1000).saturating_sub(1),
        ),
        &[("Authorization", authorization.as_str())],
        environment,
    )?;
    lark_ok(&answer, "the chat")?;
    let mut said = Vec::new();
    for item in answer
        .pointer("/data/items")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default()
    {
        // Only what a person typed. muxloom's own cards come back through the
        // same window, and routing those would be a chat talking to itself.
        if item.pointer("/sender/sender_type").and_then(Value::as_str) != Some("user")
            || item.get("deleted").and_then(Value::as_bool) == Some(true)
        {
            continue;
        }
        let Some(text) = lark_text(item) else {
            continue;
        };
        let at = item
            .get("create_time")
            .and_then(Value::as_str)
            .and_then(|ms| ms.parse().ok())
            .unwrap_or_default();
        if at <= since {
            continue;
        }
        said.push(Incoming {
            message_id: item
                .get("message_id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            reply_to: item
                .get("parent_id")
                .and_then(Value::as_str)
                .filter(|id| !id.is_empty())
                .map(str::to_string),
            at,
            text,
            context_token: String::new(),
            stale: false,
        });
    }
    Ok(said)
}

/// How long to leave a sleeping WeChat conversation alone.
///
/// WeChat's own client waits an hour, which is the right number for a client
/// that is asked to be quiet and the wrong one here: the thing that wakes the
/// conversation is the person saying something, and an hour of not looking is
/// an hour of them wondering why nobody answered. A minute is one request a
/// minute — not a load worth being careful about — and an answer that turns up
/// while they are still holding the phone.
const WECHAT_SNOOZE_MS: u64 = 60 * 1000;

/// Everything said in one WeChat chat since the last round.
///
/// The cursor is the server's, and it only moves on an answer, so a round that
/// finds nothing costs nothing and a round that is cut short loses nothing.
fn wechat_inbox(
    binding: &ChannelBinding,
    inbox: &mut Inbox,
    environment: &[(String, String)],
) -> Result<Vec<Incoming>> {
    let held = inbox.cursors.get(&binding.id);
    // No cursor at all means this dashboard has never read this chat — either
    // it was just bound, or whatever remembered where it had got to is gone.
    // Either way this round is a catch-up and nothing in it is delivered:
    // waking an agent with a conversation that happened before it existed is
    // worse than missing it.
    //
    // Read, though, rather than thrown away. What comes back on a first round
    // is most often the hello the panel has just asked for, and the token that
    // hello carries is the only thing that will ever let this bot answer. A
    // round dropped whole is a bot that stays mute until the person thinks to
    // say something a second time.
    let first = held.is_none();
    let cursor = held.cloned().unwrap_or_default();
    let round = ilink::updates(&wechat_account(binding), &cursor, environment)?;
    inbox.cursors.insert(binding.id.clone(), round.cursor);
    if round.asleep {
        inbox
            .waking
            .insert(binding.id.clone(), now_ms() + WECHAT_SNOOZE_MS);
        return Ok(Vec::new());
    }
    inbox.waking.remove(&binding.id);
    Ok(round
        .said
        .into_iter()
        .map(|said| Incoming {
            stale: first,
            // WeChat does not always number a message. The cursor is what
            // actually keeps a round from repeating itself; this only has to be
            // different from its neighbours.
            message_id: match said.message_id.is_empty() {
                true => format!("wechat-{}-{}", binding.id, said.at),
                false => said.message_id,
            },
            reply_to: said.quoted_id,
            at: said.at,
            text: said.text,
            context_token: said.context_token,
        })
        .collect())
}

/// Everything said in one chat since the last round, oldest first, whichever
/// platform it is.
fn read_chat(
    binding: &ChannelBinding,
    inbox: &mut Inbox,
    environment: &[(String, String)],
) -> Result<Vec<Incoming>> {
    match binding.kind {
        ChannelKind::WeChat => wechat_inbox(binding, inbox, environment),
        ChannelKind::Lark => {
            let Some(&since) = inbox.seen.get(&binding.id) else {
                // First sight of this chat. Start from now, for the same reason
                // as above.
                inbox.seen.insert(binding.id.clone(), now_ms());
                return Ok(Vec::new());
            };
            lark_inbox(binding, since, environment)
        }
    }
}

/// What one round of reading the chats did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct InboxRound {
    /// Messages from a human that this round acted on.
    pub read: usize,
    /// One line per message, in the words the debug log wants.
    pub routed: Vec<String>,
    pub failures: Vec<(String, String)>,
    /// Per binding, in the order they arrived: a fresher WeChat context token.
    ///
    /// This is the only thing that lets a ClawBot answer, and it is replaced by
    /// every message the human sends. The caller writes the last one into the
    /// set and bumps the revision, so that every machine in the fleet — not
    /// just the one reading the chat — can answer.
    pub refreshed: Vec<(String, String)>,
    /// Bindings WeChat has closed the conversation on. Not a failure and not
    /// something to retry harder: the person saying anything at all reopens it,
    /// and the panel's job is to say so.
    pub asleep: Vec<String>,
    /// Bindings this machine stayed silent on because another live machine
    /// holds the account lease: expected behavior when connected, worth
    /// seeing in the log, not a failure.
    pub held: Vec<String>,
}

impl InboxRound {
    pub fn busy(&self) -> bool {
        self.read > 0 || !self.failures.is_empty() || !self.refreshed.is_empty()
    }
}

/// The account a WeChat binding actually polls, for lease purposes: the bot
/// that owns the inbox, not the local name each machine gave the binding.
/// Two machines handed the same bot (same `channels.json`) share this key,
/// which is exactly the thing that must be consumed once.
fn account_key(binding: &ChannelBinding) -> String {
    for candidate in [&binding.app_id, &binding.route] {
        let trimmed = candidate.trim();
        if !trimmed.is_empty() {
            return trimmed.to_string();
        }
    }
    binding.id.clone()
}

/// One lease note read off the board, with its holder.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Leased {
    holder: String,
    until: u64,
    /// The account cursor the holder had advanced to when it posted:
    /// everything up to here was already read by this voice.
    cursor: String,
    /// When the note was posted; the newest note is the freshest word on how
    /// far the account has been read.
    ts: u64,
}

/// Read one note as a lease on `account`, or None when it is not one. The
/// holder is the note's origin — the only name both machines agree on. An
/// unreadable or foreign note is simply not a lease, never an error: old
/// muxloom versions never post these, so a note that is not ours to parse is
/// not ours to act on.
fn lease_from(message: &crate::talk::TalkMessage, account: &str) -> Option<Leased> {
    let (head, cursor) = message.text.split_once("\ncursor=")?;
    let mut lines = head.lines();
    if lines.next()? != LEASE_PREFIX {
        return None;
    }
    let mut named = String::new();
    let mut until = 0;
    for line in lines {
        if let Some(value) = line.strip_prefix("account=") {
            named = value.to_string();
        } else if let Some(value) = line.strip_prefix("until=") {
            until = value.parse().unwrap_or(0);
        }
    }
    (named == account && until > 0).then(|| Leased {
        holder: message.origin.clone(),
        until,
        cursor: cursor.to_string(),
        ts: message.ts,
    })
}

/// What one round of inbox reading does with one WeChat account.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LeaseDecision {
    /// A live machine has the account; stay silent this round.
    Yield,
    /// Read the account, then publish a lease carrying the fresh cursor.
    Consume,
    /// The lease on the board is mine, but I only just posted it and the other
    /// side may not have seen it: keep it alive and stay silent while the
    /// order settles.
    Quiet,
    /// Put a lease on the board, but stay silent this round: the board held no
    /// live lease while a peer was reachable, so a simultaneous claim may
    /// exist on the other side; wait for the settle window before speaking.
    Introduce,
}

/// The election, without the I/O.
///
/// With no live peer there is nobody to elect against, so the answer is always
/// "speak" — that is the failover half of the contract: whatever leases the
/// board still holds were posted by a machine that cannot be reached, and
/// honoring them would silence this side of a split fleet. With live peers,
/// the lease on the board decides: the smallest origin holding a live lease
/// speaks, everyone else waits. The order is a total one, so two machines
/// converge on one voice without talking about it, and a machine whose
/// controller died stops being counted on after its lease lapses. A machine
/// that just introduced stays quiet for the settle window, so two
/// simultaneous introductions cannot both answer the same batch.
fn lease_decision(
    now: u64,
    my_origin: &str,
    leases: &[Leased],
    my_intro: u64,
    peers_live: bool,
) -> LeaseDecision {
    if !peers_live {
        return LeaseDecision::Consume;
    }
    let live: Vec<&Leased> = leases.iter().filter(|l| l.until > now).collect();
    let Some(smallest) = live.iter().min_by_key(|lease| &lease.holder) else {
        return LeaseDecision::Introduce;
    };
    if smallest.holder == my_origin {
        if my_intro != 0 && now.saturating_sub(my_intro) < LEASE_SETTLE_MS {
            LeaseDecision::Quiet
        } else {
            LeaseDecision::Consume
        }
    } else {
        LeaseDecision::Yield
    }
}

/// Publish this machine's lease for the account to the board, carrying the
/// cursor the account has been read to so a successor can resume past it. A
/// post that fails is logged and shrugged off: the account stays usable, the
/// election just runs without this voice for a round.
fn publish_lease(runtime: &Runtime, account: &str, binding: &str, cursor: &str) {
    let until = now_ms().saturating_add(LEASE_TTL_MS);
    let draft = crate::talk::TalkDraft {
        scope: crate::talk::TalkScope::Path {
            machine: String::new(),
            path: LEASE_PATH.into(),
        },
        author: Default::default(),
        kind: crate::talk::TalkKind::Note,
        to: None,
        reply_to: None,
        text: format!("{LEASE_PREFIX}\naccount={account}\nuntil={until}\ncursor={cursor}"),
    };
    if let Err(error) = runtime.bridge_pool().talk_post(&Target::local(), draft) {
        debug::log(
            "channel",
            format!("{binding}: could not post its lease for {account}: {error:#}"),
        );
    }
}

/// Read the account's current leases off the board.
fn read_leases(runtime: &Runtime, account: &str) -> Result<Vec<Leased>> {
    let filter = crate::talk::TalkFilter {
        scope: Some("path".into()),
        machines: crate::talk::TalkSelector::All,
        paths: crate::talk::TalkSelector::Only {
            names: vec![LEASE_PATH.into()],
        },
        kinds: vec!["note".into()],
        limit: 64,
        ..Default::default()
    };
    let page = runtime
        .bridge_pool()
        .talk_read(&Target::local(), filter)
        .with_context(|| format!("reading the lease board for {account}"))?;
    Ok(page
        .messages
        .iter()
        .filter_map(|message| lease_from(message, account))
        .collect())
}

/// Resume where the previous voice left off.
///
/// The WeChat cursor is tracked per machine in this inbox: a machine that
/// takes over the lease only now is behind the one that consumed last, and
/// resuming from its own stale cursor would re-read — and re-answer — what the
/// other side already handled. The newest lease note carries the cursor of
/// whoever spoke most recently, so adopt it when it is someone else's and not
/// empty. A note this machine wrote itself is never newer than its own live
/// cursor; an empty cursor means the other side consumed nothing, and the
/// first-round catch-up path then applies as usual.
fn adopt_newer_cursor(
    inbox: &mut Inbox,
    binding: &ChannelBinding,
    leases: &[Leased],
    my_origin: &str,
) {
    let Some(newest) = leases.iter().max_by_key(|lease| lease.ts) else {
        return;
    };
    if newest.holder == my_origin || newest.cursor.is_empty() {
        return;
    }
    inbox
        .cursors
        .insert(binding.id.clone(), newest.cursor.clone());
}

/// Ask the board who speaks for this WeChat account this round, and make sure
/// we are on the record. Returns true when this machine stays silent.
///
/// The check happens before the chat is read on purpose: a machine that is not
/// the voice for the account does not even advance the cursor, so the batch
/// stays whole for the machine that is. A consuming round publishes its fresh
/// cursor back onto the board afterwards (see `run_inbox`), so the voice can
/// pass the account on without its successor re-reading consumed messages.
fn lease_round(
    runtime: &Runtime,
    local: Option<&crate::talk::TalkState>,
    targets: &[Target],
    inbox: &mut Inbox,
    binding: &ChannelBinding,
    round: &mut InboxRound,
) -> bool {
    let Some(local) = local else {
        debug::log(
            "channel",
            format!(
                "{}: no local board, taking account {} alone",
                binding.id,
                account_key(binding)
            ),
        );
        return false;
    };
    let account = account_key(binding);
    let leases = match read_leases(runtime, &account) {
        Ok(leases) => leases,
        Err(error) => {
            debug::log(
                "channel",
                format!(
                    "{}: lease board unreadable ({error:#}), taking account {account} alone",
                    binding.id
                ),
            );
            return false;
        }
    };
    let now = now_ms();
    let decision = lease_decision(
        now,
        &local.origin,
        &leases,
        inbox.lease_claims.get(&binding.id).copied().unwrap_or(0),
        peers_are_live(targets),
    );
    match decision {
        LeaseDecision::Yield => {
            debug::log(
                "channel",
                format!(
                    "{}: holding — another live machine has account {account}",
                    binding.id
                ),
            );
            round.held.push(binding.id.clone());
            true
        }
        LeaseDecision::Consume => {
            adopt_newer_cursor(inbox, binding, &leases, &local.origin);
            false
        }
        LeaseDecision::Quiet => {
            let cursor = inbox.cursors.get(&binding.id).cloned().unwrap_or_default();
            publish_lease(runtime, &account, &binding.id, &cursor);
            debug::log(
                "channel",
                format!(
                    "{}: settle window for account {account} — keeping the lease, \
                     staying silent one more round",
                    binding.id
                ),
            );
            round.held.push(binding.id.clone());
            true
        }
        LeaseDecision::Introduce => {
            adopt_newer_cursor(inbox, binding, &leases, &local.origin);
            inbox.lease_claims.insert(binding.id.clone(), now);
            let cursor = inbox.cursors.get(&binding.id).cloned().unwrap_or_default();
            publish_lease(runtime, &account, &binding.id, &cursor);
            debug::log(
                "channel",
                format!(
                    "{}: introducing account {account} while a peer is live — \
                     quiet for the settle window so the order settles",
                    binding.id
                ),
            );
            round.held.push(binding.id.clone());
            true
        }
    }
}

/// The message an answer should quote back, when the message just handled was
/// itself a quote.
///
/// Two things have to hold. The quote has to name a message this side sent —
/// its reference matches a receipt — or there is no exchange of ours to stay
/// in; and WeChat has to have numbered the human's own message, since a
/// synthetic catch-up id names a message the platform never gave a number and
/// so nothing can point at it. The answer quotes the human's message rather
/// than the one they quoted: their quote is drawn inside their message on the
/// phone, so pointing at theirs keeps every level of the thread one tap away.
fn quoting_target(message: &Incoming, inbox: &Inbox) -> Option<String> {
    message
        .reply_to
        .as_deref()
        .filter(|quoted| inbox.sender_of(quoted).is_some())?;
    let numbered =
        !message.message_id.is_empty() && message.message_id.chars().all(|c| c.is_ascii_digit());
    numbered.then(|| message.message_id.clone())
}

/// Read every chat a human can answer through, and put what they said where it
/// belongs.
///
/// Polling rather than a socket or a callback, and deliberately: a webhook
/// needs an address on the public internet, which most of the machines muxloom
/// runs on do not have and should not be given. The cost is that an answer
/// takes a few seconds, which the panel says out loud.
pub fn run_inbox(
    runtime: &Runtime,
    targets: &[Target],
    set: &ChannelSet,
    inbox: &mut Inbox,
    environment: &[(String, String)],
    config: &crate::config::Config,
) -> InboxRound {
    let mut round = InboxRound::default();
    // Listening asks less than sending does. A WeChat bot that has never been
    // greeted cannot say a word, and the only way it ever will is by being
    // listened to until somebody greets it.
    let listening: Vec<&ChannelBinding> = set
        .bindings
        .iter()
        .filter(|binding| binding.kind.listens() && binding.reachable().is_ok())
        .collect();
    if listening.is_empty() {
        return round;
    }
    let now = now_ms();
    // How the board names this machine: the lease is keyed by origin, and the
    // reply signature shows the label on the person's phone.
    let local = runtime
        .bridge_pool()
        .talk_status(&Target::local(), None)
        .ok();
    let local_name = local
        .as_ref()
        .map(|state| {
            if state.label.trim().is_empty() {
                state.origin.clone()
            } else {
                state.label.clone()
            }
        })
        .unwrap_or_default();
    let mut desk = Desk::new(runtime, targets, config);
    for binding in listening {
        if inbox
            .waking
            .get(&binding.id)
            .is_some_and(|&until| now < until)
        {
            round.asleep.push(binding.id.clone());
            continue;
        }
        // WeChat: one voice per account while the fleet is connected. The
        // lease holder polls; the others stay silent, so the batch stays whole
        // for whoever answers.
        if binding.kind == ChannelKind::WeChat
            && lease_round(runtime, local.as_ref(), targets, inbox, binding, &mut round)
        {
            continue;
        }
        let said = match read_chat(binding, inbox, environment) {
            Ok(said) => said,
            Err(error) => {
                round
                    .failures
                    .push((binding.id.clone(), format!("{error:#}")));
                continue;
            }
        };
        if binding.kind == ChannelKind::WeChat {
            // Publish the lease with the cursor the read just advanced, so a
            // successor resumes past what was already consumed.
            publish_lease(
                runtime,
                &account_key(binding),
                &binding.id,
                inbox
                    .cursors
                    .get(&binding.id)
                    .map(String::as_str)
                    .unwrap_or_default(),
            );
        }
        if inbox.waking.contains_key(&binding.id) {
            round.asleep.push(binding.id.clone());
        }
        for message in said {
            let Some(answering) = absorb(&message, binding, inbox, &mut round) else {
                continue;
            };
            round.read += 1;
            // A human reading the chat wants to know the agent saw what they
            // said before the agent has had time to answer. Lark can mark that
            // on the message itself, with a reaction, so the person gets "seen"
            // without the bot having to say a word. WeChat has no such mark;
            // its receipt is the eventual answer, which needs no extra message.
            if binding.kind == ChannelKind::Lark {
                if let Err(error) = ack_lark(binding, &message.message_id, environment) {
                    crate::debug::log(
                        "channel",
                        format!("seen mark through {} failed: {error:#}", binding.id),
                    );
                }
            }
            let answer = handle(&mut desk, binding, &message, inbox);
            round.routed.push(format!("{}: {answer}", binding.id));
            // The answer is itself something a human can reply to, so it is
            // remembered under whoever it is about — an agent's name in a
            // receipt is a handle, not just a report. The signature also names
            // the sending machine and counts its replies, so two bound
            // machines answering the same chat stay tellable apart.
            let receipt = Outgoing {
                title: String::new(),
                text: answer,
                signature: inbox.reply_signature(&binding.id, &local_name),
            };
            // When the message just handled quoted one of our own, the answer
            // quotes theirs back: the thread stays visible as the chain it is
            // on the phone, and the next quote — theirs of this answer —
            // matches the receipt below and finds this same agent again.
            let quoting = match binding.kind {
                ChannelKind::WeChat => quoting_target(&message, inbox),
                ChannelKind::Lark => None,
            };
            match send_reply(&answering, &receipt, quoting.as_deref(), environment) {
                Ok(sent) if !sent.message_id.is_empty() => {
                    if let Some(who) = desk.last_agent.clone() {
                        inbox.remember(ChannelReceipt {
                            channel: binding.id.clone(),
                            message_id: sent.message_id,
                            machine: who.machine,
                            session_id: who.session_id,
                            label: who.label,
                        });
                    }
                }
                Ok(_) => {}
                Err(error) => round
                    .failures
                    .push((binding.id.clone(), format!("{error:#}"))),
            }
        }
    }
    for line in &round.routed {
        debug::log("channel", line.clone());
    }
    for (binding, error) in &round.failures {
        debug::log("channel", format!("{binding}: {error}"));
    }
    round
}

/// Take everything off one inbound message that is worth keeping, and say
/// whether anybody should be told about it.
///
/// `Some` is the binding to answer through — the one holding the token off this
/// very message, since anything older may already have been spent. `None` means
/// the message has been accounted for and nothing more happens to it.
///
/// The order is the point. A message from before this dashboard was listening
/// is not worth waking an agent with, and a message already dealt with must not
/// be dealt with twice — but both still move the bookkeeping on, and a WeChat
/// one still carries the one thing that ever lets its bot speak. Refusing to
/// look at a message you have decided not to deliver is how a bot that has
/// been said hello to stays mute.
fn absorb(
    message: &Incoming,
    binding: &ChannelBinding,
    inbox: &mut Inbox,
    round: &mut InboxRound,
) -> Option<ChannelBinding> {
    let newest = inbox.seen.entry(binding.id.clone()).or_default();
    *newest = (*newest).max(message.at);
    if !message.context_token.is_empty() {
        round
            .refreshed
            .push((binding.id.clone(), message.context_token.clone()));
    }
    if inbox.handled.contains(&message.message_id) {
        return None;
    }
    inbox.mark(&message.message_id);
    if message.stale {
        return None;
    }
    Some(match message.context_token.is_empty() {
        true => binding.clone(),
        false => ChannelBinding {
            context_token: message.context_token.clone(),
            ..binding.clone()
        },
    })
}

/// The fleet, as one round of reading needs it: listed once, however many
/// messages ask about it, and not at all when none does.
struct Desk<'a> {
    runtime: &'a Runtime,
    config: &'a crate::config::Config,
    machines: Vec<Target>,
    sessions: Option<Vec<Correspondent>>,
    author: Option<crate::talk::TalkAuthor>,
    /// Who the message just handled was about, so the receipt can be replied to.
    last_agent: Option<Correspondent>,
}

impl<'a> Desk<'a> {
    fn new(runtime: &'a Runtime, targets: &[Target], config: &'a crate::config::Config) -> Self {
        let local = Target::local();
        let machines = std::iter::once(local.clone())
            .chain(targets.iter().filter(|it| it.id != local.id).cloned())
            .collect();
        Self {
            runtime,
            config,
            machines,
            sessions: None,
            author: None,
            last_agent: None,
        }
    }

    fn sessions(&mut self) -> &[Correspondent] {
        self.sessions.get_or_insert_with(|| {
            let pool = self.runtime.bridge_pool();
            let mut found = Vec::new();
            for machine in &self.machines {
                let Ok(sessions) = pool.list_sessions(machine) else {
                    continue;
                };
                found.extend(
                    sessions
                        .into_iter()
                        .filter(|it| !it.temporary)
                        .map(|session| Correspondent {
                            machine: machine.id.clone(),
                            session_id: session.id,
                            // The runtime's own title for the conversation
                            // stands in for a session nobody has named: it is
                            // what the dashboard shows, and a chat that calls
                            // the same agent something else is a chat the
                            // human has to translate.
                            label: match session.label.trim() {
                                "" => session.title.clone().unwrap_or_default(),
                                label => label.to_string(),
                            },
                            path: session.path.clone(),
                            alive: !session.dead && session.pid.is_some(),
                            working: session.working,
                            needs_attention: session.needs_attention,
                            recap: session.recap.clone(),
                            parent: session.parent.clone(),
                        }),
                );
            }
            found
        })
    }

    /// Which machine a session is on. When the caller has remembered a machine
    /// that is still among this desk's reachable targets, trust it — it is what
    /// a receipt saved, and the cheap path. But a receipt is written once and
    /// answered later, and a session may have moved or been resumed under a
    /// different machine name in between, so a remembered machine that no
    /// longer resolves is not an end of the road: the live session list knows
    /// where the session really is, and matching by id is the source of truth.
    fn locate(&mut self, who: &Correspondent) -> Option<Correspondent> {
        if !who.machine.is_empty() && self.target(&who.machine).is_some() {
            return Some(who.clone());
        }
        self.sessions()
            .iter()
            .find(|it| it.session_id == who.session_id)
            .cloned()
    }

    fn target(&self, machine: &str) -> Option<&Target> {
        self.machines.iter().find(|it| it.id == machine)
    }

    /// Machines in the stable order `/list` numbers by. Sorted by id so any
    /// node computing it reaches the same numbering: whatever the local
    /// session-dialler happens to have listed first does not leak into what a
    /// human is asked to type. `local` sorts by its own id like everything
    /// else.
    fn machines_ordered(&self) -> Vec<Target> {
        let mut machines = self.machines.clone();
        machines.sort_by(|a, b| a.id.cmp(&b.id));
        machines
    }

    /// The agents on one machine exactly as `/list` prints them: the active
    /// ones an agent did not start, in folder order. `/select <machine>-<n>`
    /// counts along this same list, so a number read off the screen and a
    /// number typed back mean the same agent. Anything else - counting the dead
    /// ones in, or counting them in the order the daemon happens to list them -
    /// aims the chat at whoever is sitting at that offset instead, which is a
    /// stranger.
    ///
    /// Subagents are left out on purpose. A phone is not a dashboard: work an
    /// agent handed to a subagent is still that agent's to answer for, so the
    /// person reading this wants the agent they gave the work to, and a list
    /// that grows by five every time one of them fans out is a list nobody can
    /// find that agent in. Nothing is hidden by it — the dashboard shows them
    /// all, indented under whoever started them, and a chat already aimed at
    /// one goes on reaching it.
    fn listed_agents(&mut self, machine: &str) -> Vec<Correspondent> {
        let mut agents: Vec<Correspondent> = self
            .sessions()
            .iter()
            .filter(|it| it.machine == machine && it.alive && it.parent.is_none())
            .cloned()
            .collect();
        // Sorted by folder alone, and stably: within a folder they stay in the
        // order the machine listed them.
        agents.sort_by(|left, right| folder_of(left).cmp(folder_of(right)));
        agents
    }

    /// How many agents on one machine are running under another agent, which
    /// is exactly what `listed_agents` left out. Said as a count rather than
    /// as rows: a person reading a chat wants to know the work is being done
    /// without having to read the name of everyone doing it.
    fn helpers_on(&mut self, machine: &str) -> usize {
        self.sessions()
            .iter()
            .filter(|it| it.machine == machine && it.alive && it.parent.is_some())
            .count()
    }

    /// Resolve a `/select <machine>-<agent>` pair of 1-based numbers to the
    /// agent they name, from the same orderings `/list` printed.
    fn resolve_numbered(&mut self, machine_no: usize, agent_no: usize) -> Option<Correspondent> {
        let machines = self.machines_ordered();
        let target = machines.get(machine_no.checked_sub(1)?)?.clone();
        let agents = self.listed_agents(&target.id);
        agents.get(agent_no.checked_sub(1)?).cloned()
    }

    /// Parse a `/select machine-agent` string of two numbers and resolve it.
    fn resolve_numbered_from(&mut self, words: &str) -> Option<Correspondent> {
        let (machine, agent) = words.trim().split_once('-')?;
        let machine: usize = machine.trim().parse().ok()?;
        let agent: usize = agent.trim().parse().ok()?;
        self.resolve_numbered(machine, agent)
    }

    /// Start a fresh agent session of `kind` in `folder` on `target`. When
    /// `temporary`, the daemon gives it a private scratch folder that is
    /// removed with the session, whatever folder was named. `initial_prompt`
    /// seeds a fresh session, and `label` names it. A launch a person asks for
    /// from a chat has no parent: it is their own piece of work, not a
    /// subagent of anything.
    ///
    /// The command and environment are the ones configured for that machine —
    /// a chat launching on `gpu-box` must get the same codex path the dashboard
    /// would give it there, not this controller's.
    fn launch(
        &self,
        target: &Target,
        kind: crate::model::AgentKind,
        folder: &str,
        temporary: bool,
        initial_prompt: Option<String>,
        label: &str,
    ) -> Result<String> {
        use crate::model::LaunchRequest;
        let request = LaunchRequest {
            target: target.clone(),
            kind,
            path: folder.to_string(),
            label: label.to_string(),
            temporary,
            resume_id: None,
            initial_prompt,
            parent: None,
        };
        let command = self.config.command_for(&target.id, kind);
        let environment = self.config.environment_for(&target.id).unwrap_or_default();
        self.runtime.launch(&request, command, &environment)
    }

    /// Send the interrupt byte to an agent's session, the way an attached
    /// terminal's Ctrl-C would, to stop whatever turn it is running.
    fn stop(&self, who: &Correspondent) -> Result<()> {
        let target = self
            .target(&who.machine)
            .ok_or_else(|| anyhow!("{} is on a machine we cannot reach", who.machine))?;
        self.runtime
            .bridge_pool()
            .send_input(target, who.session_id.clone(), vec![3]) // ^C
    }

    /// A human speaking, as the board records them. Asked of the local board
    /// once, because what this machine is called is not something a chat knows.
    ///
    /// `channel` is the binding they wrote from, and it travels with the words:
    /// a person on a phone reads the answer in the app they typed into, so an
    /// agent that is handed this message has to be handed the way back to it.
    fn author(&mut self, called: &str, channel: &str) -> crate::talk::TalkAuthor {
        let base = self.author.get_or_insert_with(|| {
            let (machine, machine_label) = self
                .runtime
                .bridge_pool()
                .talk_status(&Target::local(), None)
                .map(|state| (state.origin, state.label))
                .unwrap_or_default();
            crate::talk::TalkAuthor {
                machine,
                machine_label,
                voice: crate::talk::TalkVoice::default(),
            }
        });
        crate::talk::TalkAuthor {
            voice: crate::talk::TalkVoice {
                label: Some(called.to_string()),
                // The one thing an agent reading this has to know: a person
                // wrote it, so it is not another agent's suggestion.
                human: true,
                channel: Some(channel.to_string()),
                ..Default::default()
            },
            ..base.clone()
        }
    }
}

/// Render active agents grouped by the folder they run in, each carrying the
/// `<machine>-<agent>` number `/select` takes, what it is doing, and its
/// `recap` (the last thing the model said) on the line under it. A folder with
/// no path — a session that did not say where it runs — is grouped under `~`.
///
/// The whole `machine-agent` pair is printed, not the agent's ordinal alone,
/// because the pair is what a person types back. A list that prints `2` under
/// a heading and asks for `/select <machine>-<agent>` in the footer makes the
/// reader assemble the command out of two halves printed in different places,
/// on a phone, from memory.
///
/// The state is on the name's own line, not in the recap: the recap is the
/// last thing the model said, and the last thing a model said reads exactly
/// the same whether it said it ten seconds ago or yesterday.
fn folder_grouped(machine_no: usize, agents: &[&Correspondent]) -> Vec<String> {
    let mut lines = Vec::new();
    let mut heading: Option<&str> = None;
    // Counted straight along the list it was handed, and never re-ordered
    // here: the caller's order is the one `/select` counts along, and a
    // renderer that quietly sorted differently is how a number came to mean
    // one agent on the screen and another in the command.
    for (index, who) in agents.iter().enumerate() {
        let folder = folder_of(who);
        if heading != Some(folder) {
            if heading.is_some() {
                lines.push(String::new());
            }
            lines.push(folder_shown(folder));
            heading = Some(folder);
        }
        lines.push(format!(
            "  {machine_no}-{}  {} · {}",
            index + 1,
            who.list_name(),
            who.state()
        ));
        if let Some(recap) = who.recap.as_ref().filter(|r| !r.trim().is_empty()) {
            let one = recap.split('\n').next().unwrap_or("").trim();
            if !one.is_empty() {
                // Shorter than a terminal would allow: a hundred characters is
                // already three lines on a phone, and this is a glance at what
                // the agent is on, not the sentence itself.
                let recap_line = if one.chars().count() > 100 {
                    let cut: String = one.chars().take(97).collect();
                    format!("{cut}…")
                } else {
                    one.to_string()
                };
                // Three spaces, not more: four is a code block in every
                // markdown a chat renders, and a recap set in a grey monospace
                // box is not what this line is.
                lines.push(format!("   {recap_line}"));
            }
        }
    }
    if heading.is_some() {
        lines.push(String::new());
    }
    lines
}

/// The folder an agent is listed under. A session that did not say where it
/// runs is grouped under `~`, which sorts last.
fn folder_of(who: &Correspondent) -> &str {
    match who.path.trim() {
        "" => "~",
        folder => folder,
    }
}

/// The heading a folder gets in a chat: its last two components, with what was
/// dropped marked. Grouping is still by the whole path — two folders that end
/// the same way are still two folders — but the whole path is not what goes on
/// the screen. `/Users/someone/Works/Terminal` is a line and a half on a
/// phone, most of it the same prefix on every row, and the part of it that is
/// a person's home directory is not something to put in a chat at all.
fn folder_shown(folder: &str) -> String {
    let parts: Vec<&str> = folder.split('/').filter(|it| !it.is_empty()).collect();
    if parts.len() < 3 {
        return folder.trim_end_matches('/').to_string();
    }
    format!("…/{}", parts[parts.len() - 2..].join("/"))
}

/// Act on one thing a human said, and answer in the words they should see.
fn handle(
    desk: &mut Desk<'_>,
    binding: &ChannelBinding,
    message: &Incoming,
    inbox: &mut Inbox,
) -> String {
    desk.last_agent = None;
    let called = if binding.label.trim().is_empty() {
        format!("a human via {}", binding.kind.title())
    } else {
        format!("{} via {}", binding.label.trim(), binding.kind.title())
    };
    // What to call a session this chat starts. `called` is how the *person*
    // signs what they say — "Ming via WeChat" — and naming an agent after the
    // human who asked for it is how a fleet ends up with four agents all called
    // WeChat. The chat is the one true thing about it until it names itself
    // with `set_head_name`, which is what a person then reads.
    let chat_name = match binding.label.trim() {
        "" => binding.kind.title().to_string(),
        label => label.to_string(),
    };
    let (decision, text) = route(
        &message.text,
        message.reply_to.as_deref(),
        &binding.id,
        binding.kind.solo(),
        inbox,
    );
    match decision {
        Route::Who => {
            // The ones still running, which are the ones there is any point
            // aiming at. A machine that has been up for days carries dozens of
            // finished sessions, and a roster where the agents you can still
            // talk to are outnumbered several times over by the ones you
            // cannot is a roster nobody reads to the end on a phone. The same
            // goes for the subagents those running agents started: whoever
            // gave the work out is the one to ask about it. Dropping either
            // without a word would be its own kind of lie, so the counts stay.
            let (live, finished): (Vec<&Correspondent>, Vec<&Correspondent>) =
                desk.sessions().iter().partition(|who| who.alive);
            let finished = finished.len();
            let helpers = live.iter().filter(|who| who.parent.is_some()).count();
            let mut lines: Vec<String> = live
                .iter()
                .filter(|who| who.parent.is_none())
                .map(|who| format!("- {} · {}", who.name(), who.state()))
                .collect();
            if lines.is_empty() {
                lines.push("- nobody is running".into());
            }
            if helpers > 0 {
                lines.push(format!("- ({helpers} working under them)"));
            }
            if finished > 0 {
                lines.push(format!("- ({finished} finished, not listed)"));
            }
            lines.push(String::new());
            lines.push(match binding.kind.solo() {
                // A chat with one person in it needs no ceremony to answer in,
                // so the commands are offered as the extras they are.
                true => "Just type — it goes to whoever spoke to you last. \
                         `/select <name>` to aim somewhere else, `/clear` to stop aiming, \
                         `/all <text>` to put something where every agent reads it."
                    .into(),
                false => "`/select <name>` to aim this chat · `/clear` to stop · \
                          `/all <text>` for the board · replying to a card answers \
                          whoever sent it"
                    .into(),
            });
            lines.join("\n")
        }
        Route::List(arg) if arg.trim().is_empty() => {
            // Numbered over the stable ordering, so the same list reads the
            // same wherever it is printed and a number a human re-types names
            // the same machine on any node.
            let machines = desk.machines_ordered();
            if machines.is_empty() {
                "· no machines to list".into()
            } else {
                // With a count beside each, because a bare list of machine
                // names does not say which one to open, and opening the wrong
                // one on a phone costs a round trip to find out it was empty.
                let mut lines: Vec<String> = Vec::new();
                for (index, m) in machines.iter().enumerate() {
                    let agents = desk.listed_agents(&m.id).len();
                    lines.push(match agents {
                        0 => format!("{}  {} · no agents", index + 1, m.label_or_id()),
                        1 => format!("{}  {} · 1 agent", index + 1, m.label_or_id()),
                        n => format!("{}  {} · {n} agents", index + 1, m.label_or_id()),
                    });
                }
                lines.push(String::new());
                lines.push(
                    "`/list 1` shows that machine's agents · `/select 1-2` aims this chat at one"
                        .into(),
                );
                lines.join("\n")
            }
        }
        Route::List(arg) => {
            // A machine number lists the *active* agents on it, grouped by the
            // folder they run in, each carrying a glance at what it is doing.
            match arg.trim().parse::<usize>() {
                Ok(number) => {
                    let machines = desk.machines_ordered();
                    match machines.get(number.checked_sub(1).unwrap_or(usize::MAX)) {
                        Some(m) => {
                            // The very list `/select <machine>-<n>` counts
                            // along, printed with those numbers on it.
                            let agents = desk.listed_agents(&m.id);
                            let active: Vec<&Correspondent> = agents.iter().collect();
                            if active.is_empty() {
                                format!("· {} has no active agents", m.label_or_id())
                            } else {
                                // The example is a real number off this very
                                // list rather than `<machine>-<agent>`: a
                                // placeholder is one more thing to work out on
                                // a phone, and the first row is always there.
                                let mut lines = folder_grouped(number, &active);
                                lines.push(format!(
                                    "`/select {number}-1` aims this chat · `/new {number}` starts a fresh agent here"
                                ));
                                lines.push(match desk.helpers_on(&m.id) {
                                    0 => "· active agents only".to_string(),
                                    n => format!(
                                        "· active agents only, and {n} more working under the ones listed"
                                    ),
                                });
                                lines.join("\n")
                            }
                        }
                        None => format!("· no machine numbered `{arg}` — `/list` shows them"),
                    }
                }
                Err(_) => format!(
                    "· `/list` takes a machine number from `/list` — `{}` is not one",
                    arg.trim()
                ),
            }
        }
        Route::Help => {
            let mut lines: Vec<String> = vec![
                "Commands this chat understands:".into(),
                "  /call <text>        run <text> with a fresh one-shot agent (the one /call last used), once".into(),
                "  /list              the machines, with how many agents are running on each".into(),
                "  /list <num>        one machine's agents, grouped by folder, with what each is doing".into(),
                "  /select <num>-<n>  aim this chat at one agent, until another writes or /clear".into(),
                "  /select <name>     aim at an agent by name".into(),
                "  /who               every running agent everywhere, in one flat list".into(),
                "  /clear             stop aiming; plain messages go to the board".into(),
                "  /all <text>        put something on the board every agent reads".into(),
                "  /new <machine>     start a fresh agent there, in a scratch folder, aimed at this chat".into(),
                "  /help              this list".into(),
            ];
            lines.push(String::new());
            lines.push(match binding.kind.solo() {
                true => "Just type a plain sentence and it goes to whoever you last talked to — /select picks that up front, and reply to a card to answer that agent instead.".into(),
                false => "Just type a plain sentence and it goes where this chat is aimed, or to the board if nothing is. A reply to a card always reaches the agent who sent it.".into(),
            });
            lines.join("\n")
        }
        Route::New(arg) => {
            // Which machine has to be said. A chat is read on a phone, from
            // anywhere, and the controller's own current directory is not a
            // place the person can see; guessing it means an agent quietly
            // starting on the wrong side of the fleet.
            let machines = desk.machines_ordered();
            let Ok(number) = arg.trim().parse::<usize>() else {
                let mut lines = vec![match arg.trim().is_empty() {
                    true => "· `/new <machine>` — which machine:".to_string(),
                    false => format!(
                        "· `/new` takes a machine number — `{}` is not one:",
                        arg.trim()
                    ),
                }];
                lines.extend(
                    machines
                        .iter()
                        .enumerate()
                        .map(|(index, m)| format!("{}  {}", index + 1, m.label_or_id())),
                );
                return lines.join("\n");
            };
            let Some(target) = machines
                .get(number.checked_sub(1).unwrap_or(usize::MAX))
                .cloned()
            else {
                return format!("· no machine numbered `{number}` — `/list` shows them");
            };
            // The kind is this chat's default (set by /agent), defaulting to
            // claude.
            let kind = inbox
                .default_kind
                .get(&binding.id)
                .copied()
                .unwrap_or(AgentKind::Claude);
            // In muxloom's own scratch folder, made and removed by the daemon
            // on that machine. An agent started from a chat has no project
            // behind it — nobody chose a repository, and moving into whichever
            // directory the controller happened to be launched from leaves its
            // droppings somewhere that never asked for them.
            match desk.launch(&target, kind, ".", true, None, &chat_name) {
                Ok(session_id) => {
                    // A scratch session is not in `/list`, so nothing could aim
                    // at it afterwards: aim this chat at it now, which is what
                    // somebody who just started an agent meant anyway.
                    let who = Correspondent {
                        machine: target.id.clone(),
                        session_id: session_id.clone(),
                        label: chat_name.clone(),
                        alive: true,
                        ..Default::default()
                    };
                    desk.last_agent = Some(who.clone());
                    inbox.aim(&binding.id, who);
                    format!(
                        "· started {} on {} in a scratch folder, and aimed this chat at it — just type",
                        kind.as_str(),
                        target.label_or_id()
                    )
                }
                Err(error) => format!("· could not start an agent: {error:#}"),
            }
        }
        Route::Call(text) => {
            let kind = inbox
                .last_call_kind
                .get(&binding.id)
                .copied()
                .or_else(|| inbox.default_kind.get(&binding.id).copied())
                .unwrap_or(AgentKind::Claude);
            inbox.last_call_kind.insert(binding.id.clone(), kind);
            let text = text.trim().to_string();
            let prompt = if text.is_empty() {
                None
            } else {
                Some(text.clone())
            };
            match desk.launch(&Target::local(), kind, ".", true, prompt, &chat_name) {
                Ok(_) => {
                    let what = if text.is_empty() {
                        "(no instruction given)".to_string()
                    } else {
                        text
                    };
                    format!(
                        "· {} running `{what}` in a scratch folder — one-shot",
                        kind.as_str()
                    )
                }
                Err(error) => format!("· could not start an agent: {error:#}"),
            }
        }
        Route::AgentKind(word) => {
            let word = word.trim();
            if word.is_empty() {
                let current = inbox
                    .default_kind
                    .get(&binding.id)
                    .copied()
                    .unwrap_or(AgentKind::Claude);
                format!(
                    "· this chat starts {} — `/agent claude|pi|codex|opencode` to change",
                    current.as_str()
                )
            } else {
                match parse_kind(word) {
                    Some(kind) => {
                        inbox.default_kind.insert(binding.id.clone(), kind);
                        format!("· this chat will start {} from now on", kind.as_str())
                    }
                    None => format!(
                        "· `{word}` is not one of codex / pi / claude / opencode — `/agent <kind>` to set the default"
                    ),
                }
            }
        }
        Route::Current => {
            let mut parts = Vec::new();
            let kind = inbox
                .default_kind
                .get(&binding.id)
                .copied()
                .unwrap_or(AgentKind::Claude);
            parts.push(format!("default kind: {}", kind.as_str()));
            match inbox.aimed.get(&binding.id) {
                Some(who) => parts.push(format!("aimed at: {}", who.name())),
                None => parts.push("aimed at: nothing".into()),
            }
            if let Some(k) = inbox.last_call_kind.get(&binding.id) {
                parts.push(format!("last /call: {}", k.as_str()));
            }
            parts.join(" · ")
        }
        Route::Stop => {
            // Interrupt the agent this chat is aimed at: send the interrupt
            // byte as typing, which is what an attached terminal's Ctrl-C does.
            match inbox.aimed.get(&binding.id) {
                Some(who) => match desk.stop(who) {
                    Ok(()) => format!("· interrupted {}", who.name()),
                    Err(error) => format!("· could not interrupt {}: {error:#}", who.name()),
                },
                None => "· nothing is aimed — `/select <num>-<n>` first, or reply to a card".into(),
            }
        }
        Route::Approval { id, verdict } => {
            let path = crate::approvals::Approvals::default_path();
            let mut ledger = crate::approvals::Approvals::load(&path);
            let Some(pending) = ledger.take(&id) else {
                return "· that approval is already settled or was never asked".into();
            };
            let out = match verdict {
                crate::approvals::Verdict::No => "· denied".to_string(),
                crate::approvals::Verdict::Yes => {
                    ledger.grant_once(&pending.session, &pending.machine, &pending.tool);
                    format!(
                        "· allowed {} once — tell the agent to run it again",
                        pending.tool
                    )
                }
                crate::approvals::Verdict::Always => {
                    if crate::relay::reminder_allowed(&pending.tool) {
                        ledger.remember(&pending.session, &pending.machine, &pending.tool);
                        format!(
                            "· allowed {} for the rest of this conversation",
                            pending.tool
                        )
                    } else {
                        // Too sensitive to remember; the one-shot is the ceiling.
                        ledger.grant_once(&pending.session, &pending.machine, &pending.tool);
                        format!(
                            "· {} is sensitive, so allowed it once only — tell the agent to run it again",
                            pending.tool
                        )
                    }
                }
            };
            if let Err(error) = ledger.save(&path) {
                return format!("· could not record that: {error:#}");
            }
            out
        }
        Route::Clear => match inbox.unaim(&binding.id) {
            Some(who) => format!("· no longer aimed at {}", who.name()),
            None => "· nothing was aimed".into(),
        },
        Route::Unknown(word) => format!(
            "· there is no `{word}` — `/who` lists what there is. To say that to an agent \
             instead, take the slash off."
        ),
        Route::Aim(words) if words.is_empty() => {
            "· `/select` needs a name — `/who` lists them".into()
        }
        Route::Aim(words) => {
            // A numbered `machine-agent` pair, as `/list` printed it, aims at
            // one agent without needing to match names. Numbers are settled
            // before names so a session whose id looks like two numbers still
            // yields to what the human was just shown.
            if let Some(only) = desk.resolve_numbered_from(&words) {
                let name = only.name();
                desk.last_agent = Some(only.clone());
                inbox.aim(&binding.id, only.clone());
                return format!("· aimed at {name}");
            }
            // A name is matched against the running agents, the same ones
            // `/list` numbers and `/who` prints. The dead outnumber them many
            // times over on a machine that has been up a while, and a name
            // that fits both a finished session and the live one that took its
            // place must not land on the corpse - nor read as ambiguous
            // because of it.
            let (live, finished): (Vec<Correspondent>, Vec<Correspondent>) = desk
                .sessions()
                .iter()
                .filter(|who| who.answers_to(&words))
                .cloned()
                .partition(|who| who.alive);
            match live.as_slice() {
                // Saying which finished session it was is the whole answer
                // here: the human typed a name they remember, and what they
                // need to hear is that it is over, not that it never existed.
                [] if !finished.is_empty() => format!(
                    "· {} has finished, so there is nobody to aim at — `/who` lists what is running",
                    finished[0].name()
                ),
                [] => format!("· nothing here answers to `{words}` — `/who` lists what does"),
                [only] => {
                    let name = only.name();
                    desk.last_agent = Some(only.clone());
                    inbox.aim(&binding.id, only.clone());
                    format!("· aimed at {name}")
                }
                several => format!(
                    "· `{words}` fits {} of them: {}",
                    several.len(),
                    several
                        .iter()
                        .map(Correspondent::name)
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            }
        }
        Route::Agent(who) => {
            let Some(who) = desk.locate(&who) else {
                inbox.forget(&binding.id, &who.session_id);
                return format!(
                    "· {} is not running any more, so that went nowhere. `/who` lists what is.",
                    who.name()
                );
            };
            let Some(target) = desk.target(&who.machine).cloned() else {
                return format!("· {} is on a machine muxloom cannot reach", who.name());
            };
            // When they replied by quoting one of this agent's own messages,
            // the quoted platform id is the one thing the agent needs to answer
            // quoting the same exchange, and nothing else carries it across the
            // delivery. It travels on the author's return address rather than
            // pasted into the message: the words in the body are the person's,
            // and muxloom adding a line to them is muxloom putting words in
            // their mouth. Only a quote that matches this agent's own receipt
            // counts — a quote of somebody else's message is not theirs to
            // answer quoting.
            let quoted = message.reply_to.as_deref().filter(|quoted| {
                inbox
                    .sender_of(quoted)
                    .is_some_and(|from| from.session_id == who.session_id)
            });
            let mut author = desk.author(&called, &binding.id);
            author.voice.channel_quote = quoted.map(str::to_string);
            let draft = crate::talk::TalkDraft {
                scope: crate::talk::TalkScope::Machine {
                    machine: String::new(),
                },
                author,
                kind: crate::talk::TalkKind::Direct,
                to: Some(crate::talk::TalkAddress {
                    machine: String::new(),
                    session_id: who.session_id.clone(),
                }),
                reply_to: None,
                text,
            };
            desk.last_agent = Some(who.clone());
            match desk.runtime.bridge_pool().talk_deliver(
                &target,
                draft,
                crate::talk::TalkDeliver::Auto,
                true,
            ) {
                Ok((_, delivery, reason)) => match reason {
                    Some(reason) => format!("→ {} ({delivery}: {reason})", who.name()),
                    None => format!("→ {} ({delivery})", who.name()),
                },
                Err(error) => format!("· {} did not get that: {error:#}", who.name()),
            }
        }
        Route::Board { asked } => {
            let draft = crate::talk::TalkDraft {
                scope: crate::talk::TalkScope::Machine {
                    machine: String::new(),
                },
                author: desk.author(&called, &binding.id),
                kind: crate::talk::TalkKind::Message,
                to: None,
                reply_to: None,
                text,
            };
            match desk
                .runtime
                .bridge_pool()
                .talk_post(&Target::local(), draft)
            {
                Ok(_) if asked => "· on the board, where every agent reads it".into(),
                Ok(_) => "· on the board — nothing is aimed yet. `/who` lists who is here.".into(),
                Err(error) => format!("· the board did not take that: {error:#}"),
            }
        }
    }
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
            route_label: "muxloom".into(),
            ..Default::default()
        }
    }

    fn wechat(id: &str) -> ChannelBinding {
        ChannelBinding {
            id: id.into(),
            kind: ChannelKind::WeChat,
            label: "me".into(),
            app_id: "bot_9@im.bot".into(),
            secret: "bot-token".into(),
            route: "u_1@im.wechat".into(),
            base_url: "https://ilinkai.weixin.qq.com".into(),
            context_token: "ctx-1".into(),
            ..Default::default()
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
    fn an_app_cc_connect_already_has_is_offered_wherever_it_keeps_it() {
        let path = std::env::temp_dir().join(format!(
            "muxloom-cc-connect-{}-{}.toml",
            std::process::id(),
            line!()
        ));
        // Nested, because cc-connect's own layout is not muxloom's to promise
        // and a key that moves into a table should not silently stop working.
        fs::write(
            &path,
            "log_level = \"info\"\n\n[feishu]\napp_id = \"cli_9\"\napp_secret = \"shhh\"\n\
             [feishu.defaults]\nchat_id = \"oc_1\"\n",
        )
        .unwrap();
        let borrowed = borrow_cc_connect(&path).expect("an app is there to borrow");
        assert_eq!(borrowed.app_id, "cli_9");
        assert_eq!(borrowed.secret, "shhh");
        assert_eq!(borrowed.route, "oc_1");
        assert_eq!(borrowed.from, path);

        // Half an app is nothing to offer: prefilling one field of two reads as
        // muxloom knowing something it does not.
        fs::write(&path, "[feishu]\napp_id = \"cli_9\"\n").unwrap();
        assert_eq!(borrow_cc_connect(&path), None);
        let _ = fs::remove_file(&path);
        assert_eq!(
            borrow_cc_connect(&path),
            None,
            "and no file is not an error"
        );
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

        set.bindings.push(wechat("wechat-1"));
        let ambiguous = set.pick(None).unwrap_err().to_string();
        assert!(ambiguous.contains("lark-1") && ambiguous.contains("wechat-1"));
        set.bindings[1].preferred = true;
        assert_eq!(set.pick(None).unwrap().id, "wechat-1");

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
        assert_eq!(set.mint_id(ChannelKind::WeChat), "wechat-1");
    }

    #[test]
    fn an_incomplete_binding_says_what_would_finish_it() {
        let mut binding = lark("lark-1");
        binding.route.clear();
        let error = binding.ready().unwrap_err().to_string();
        assert!(error.contains("no chat to post into"), "{error}");
        binding.secret.clear();
        let error = binding.ready().unwrap_err().to_string();
        assert!(error.contains("app secret"), "{error}");
        // Missing credentials are missing before a missing destination is:
        // there is no point telling somebody to pick a chat with an app that
        // could not list one.
        assert!(binding.reachable().is_err());

        let bot = wechat("wechat-1");
        assert!(bot.ready().is_ok());
        // A bot nobody has greeted can be listened to but cannot speak, and the
        // difference has to be said in words a person can act on, because the
        // action is theirs: they have to say something.
        let unspoken = ChannelBinding {
            context_token: String::new(),
            ..bot
        };
        assert!(
            unspoken.reachable().is_ok(),
            "it must still be polled — that is how it earns a token"
        );
        let error = unspoken.ready().unwrap_err().to_string();
        assert!(error.contains("Say anything at all to the bot"), "{error}");

        // Before the scan there is nothing at all.
        let empty = ChannelBinding {
            id: "wechat-2".into(),
            kind: ChannelKind::WeChat,
            ..Default::default()
        };
        let error = empty.reachable().unwrap_err().to_string();
        assert!(
            error.contains("bot token") && error.contains("press c"),
            "{error}"
        );
    }

    #[test]
    fn a_leading_heading_becomes_the_title_unless_there_already_is_one() {
        let message = Outgoing {
            title: String::new(),
            text: "# 构建完成\n\n42 tests passed.".into(),
            signature: "*— gpu-1 · claude*".into(),
        };
        let (title, body) = message.compose(LARK_LIMIT);
        assert_eq!(title, "构建完成");
        assert_eq!(body, "*— gpu-1 · claude*\n\n42 tests passed.");

        let named = Outgoing {
            title: "Nightly".into(),
            ..message.clone()
        };
        let (title, body) = named.compose(LARK_LIMIT);
        assert_eq!(title, "Nightly");
        assert!(
            body.trim_start().starts_with("*— gpu-1 · claude*"),
            "the source must stay on top: {body}"
        );
        assert!(
            body.contains("# 构建完成"),
            "a heading the title did not take must stay in the text: {body}"
        );

        // Nothing that looks like a heading, nothing taken out of the text.
        let plain = Outgoing {
            title: String::new(),
            text: "#not a heading".into(),
            signature: String::new(),
        };
        assert_eq!(
            plain.compose(LARK_LIMIT),
            (String::new(), "#not a heading".into())
        );
    }

    #[test]
    fn a_long_message_is_cut_to_fit_and_still_says_who_sent_it() {
        let message = Outgoing {
            title: "Report".into(),
            text: "あ".repeat(4096),
            signature: "*— gpu-1*".into(),
        };
        let (_, body) = message.compose(WECHAT_LIMIT);
        assert!(body.len() <= WECHAT_LIMIT, "{} bytes", body.len());
        assert!(body.contains("cut to fit"));
        assert!(
            body.starts_with("*— gpu-1*"),
            "the source must survive the cut: {body}"
        );
        // A cut that landed inside one of those three-byte characters would
        // have panicked on the way out of `clip`.
        assert!(body.contains('あ'));
    }

    /// The platform cut above is the last resort, for a message that got this
    /// far. An agent's message never should: it is handed back instead, with
    /// enough of a reason to write a shorter one.
    #[test]
    fn a_message_too_long_to_read_on_a_phone_is_refused_rather_than_trimmed() {
        let message = Outgoing {
            title: "Report".into(),
            text: "あ".repeat(READABLE_LIMIT + 1),
            signature: "*— gpu-1*".into(),
        };
        let refusal = format!("{:#}", refuse_if_too_long(&message).unwrap_err());
        assert!(
            refusal.contains(&(READABLE_LIMIT + 1).to_string()),
            "{refusal}"
        );
        assert!(refusal.contains(&READABLE_LIMIT.to_string()), "{refusal}");
        assert!(refusal.contains("Nothing was sent"), "{refusal}");
    }

    /// Characters, not bytes. A message in Chinese is three times the bytes of
    /// the same message in English and is not three times as long to read, so
    /// counting bytes would hold half the fleet's users to a third of the cap.
    #[test]
    fn the_cap_counts_characters_so_chinese_is_not_charged_three_times() {
        let message = Outgoing {
            title: "报告".into(),
            text: "字".repeat(READABLE_LIMIT),
            signature: "*— gpu-1*".into(),
        };
        assert!(message.text.len() > READABLE_LIMIT, "three bytes each");
        refuse_if_too_long(&message).expect("as many characters as the cap allows");
    }

    /// A header is a few words. One that needs a second line is a first line.
    #[test]
    fn a_title_longer_than_a_header_is_refused_too() {
        let message = Outgoing {
            title: "x".repeat(TITLE_LIMIT + 1),
            text: "short".into(),
            signature: "*— gpu-1*".into(),
        };
        let refusal = format!("{:#}", refuse_if_too_long(&message).unwrap_err());
        assert!(refusal.contains("title"), "{refusal}");
    }

    #[test]
    fn the_card_is_the_only_structure_that_renders_a_whole_message() {
        let card = lark_card("Nightly", "# heading\n\n| a | b |\n|---|---|\n| 1 | 2 |");
        assert_eq!(card["schema"], "2.0");
        assert_eq!(card["body"]["elements"][0]["tag"], "markdown");
        assert!(
            card["body"]["elements"][0]["content"]
                .as_str()
                .unwrap()
                .contains("| a | b |")
        );
        assert_eq!(card["header"]["title"]["content"], "Nightly");
        // No title, no header: an empty one would render as a bare grey bar.
        assert!(lark_card("", "hello").get("header").is_none());
    }

    #[test]
    fn the_bot_link_carries_the_app_id_and_nothing_else() {
        // One parameter is the whole of it. The temptation is to add the chat
        // or the tenant, and an AppLink with anything else on it is one the
        // client declines to open.
        assert_eq!(
            lark_bot_link("  cli_9c21a4767c305107\n"),
            "https://applink.feishu.cn/client/bot/open?appId=cli_9c21a4767c305107"
        );
    }

    #[test]
    fn a_direct_message_is_told_apart_from_a_group() {
        // The chooser shows the bot's chats in one list, and a private message
        // and a room are named the same way — so the row must say which it is.
        let dm = Chat {
            id: "oc_1".into(),
            name: "hanxiao".into(),
            chat_mode: "p2p".into(),
        };
        let room = Chat {
            id: "oc_2".into(),
            name: "研发日常".into(),
            chat_mode: "group".into(),
        };
        assert!(
            dm.label().starts_with("💬"),
            "direct message: {}",
            dm.label()
        );
        assert!(dm.label().ends_with("hanxiao"));
        assert!(room.label().starts_with("👥"), "group: {}", room.label());
        assert!(room.label().ends_with("研发日常"));
    }

    #[test]
    fn a_refusal_says_what_the_platform_said() {
        let error = lark_ok(
            &json!({ "code": 230001, "msg": "bot is not in the chat" }),
            "the message",
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("bot is not in the chat"), "{error}");
        assert!(error.contains("230001"), "{error}");
        assert!(lark_ok(&json!({ "code": 0 }), "the message").is_ok());
    }

    #[test]
    fn markdown_is_flattened_for_a_chat_that_renders_none_of_it() {
        let written = "# Nightly\n\
                       The **lexer** rewrite is `done`.\n\n\
                       - 42 tests [passed](https://ci.example.com/9)\n\
                       - 0 failed\n\n\
                       ```\nassert_eq!(1, 1);\n```\n";
        assert_eq!(
            plain(written),
            "Nightly\n\
             The lexer rewrite is done.\n\n\
             · 42 tests passed (https://ci.example.com/9)\n\
             · 0 failed\n\n\
             assert_eq!(1, 1);"
        );
        // A heading gets the only emphasis plain text has: a line to itself.
        assert_eq!(plain("before\n## after"), "before\n\nafter");
        // Indentation is structure, so it survives; the marker in front of it
        // is decoration, so it does not.
        assert_eq!(plain("  - nested"), "  · nested");
        // A link with no words is its address, which is better than nothing.
        assert_eq!(plain("see [](https://x.example)"), "see https://x.example");
        // A bracket that is not a link is left exactly as it was typed.
        assert_eq!(plain("array[0] is fine"), "array[0] is fine");
        assert_eq!(plain(""), "");
    }

    #[test]
    fn a_binding_reads_back_as_where_it_goes_rather_than_as_an_id() {
        assert_eq!(lark("lark-1").describes(), "Lark / 飞书 · muxloom");
        assert_eq!(
            wechat("wechat-1").describes(),
            "WeChat / 微信 · your WeChat"
        );
        // A binding half made says so, rather than showing an empty space that
        // reads as a bug.
        let unscanned = ChannelBinding {
            kind: ChannelKind::WeChat,
            ..Default::default()
        };
        assert_eq!(unscanned.destination(), "not scanned yet");
        let unpicked = ChannelBinding {
            kind: ChannelKind::Lark,
            ..Default::default()
        };
        assert_eq!(unpicked.destination(), "no chat picked yet");
    }

    #[test]
    fn a_context_token_is_as_secret_as_the_token_it_travels_with() {
        let set = ChannelSet {
            revision: 1,
            bindings: vec![wechat("wechat-1")],
        };
        let handed_out = serde_json::to_string(&set.redacted()).unwrap();
        assert!(!handed_out.contains("bot-token"), "{handed_out}");
        assert!(
            !handed_out.contains("ctx-1"),
            "anything holding a context token can post as this bot: {handed_out}"
        );
        // What is left is still enough to tell one binding from another.
        assert!(handed_out.contains("wechat-1") && handed_out.contains("bot_9"));
    }

    #[test]
    fn one_binding_this_build_cannot_read_does_not_take_the_others_with_it() {
        let path = std::env::temp_dir().join(format!(
            "muxloom-channels-{}-{}.json",
            std::process::id(),
            line!()
        ));
        // The middle one is a kind that was removed between versions; the last
        // is one a newer dashboard invented. Both are dropped, and the one this
        // build understands survives.
        fs::write(
            &path,
            r#"{"revision":7,"bindings":[
                 {"id":"lark-1","kind":"lark","app_id":"cli_9","secret":"s","route":"oc_1"},
                 {"id":"wecom-1","kind":"we_com","secret":"key"},
                 {"id":"signal-1","kind":"signal","secret":"key"}]}"#,
        )
        .unwrap();
        let set = ChannelSet::load(&path).unwrap();
        assert_eq!(set.revision, 7, "the revision must not go backwards");
        assert_eq!(set.names(), "lark-1");
        let _ = fs::remove_file(&path);
    }

    fn who(session_id: &str, label: &str) -> Correspondent {
        Correspondent {
            machine: "seed".into(),
            session_id: session_id.into(),
            label: label.into(),
            ..Default::default()
        }
    }

    #[test]
    fn where_a_human_sentence_goes_is_settled_before_anyone_is_asked() {
        let mut inbox = Inbox::default();
        // Nothing aimed and nothing replied to: it goes where every agent can
        // find it rather than nowhere.
        assert_eq!(
            route("the tests are red again", None, "lark-1", false, &inbox),
            (
                Route::Board { asked: false },
                "the tests are red again".into()
            )
        );
        // The commands, with the command word off the front of what is left.
        assert_eq!(
            route("/select lexer", None, "lark-1", false, &inbox),
            (Route::Aim("lexer".into()), "lexer".into())
        );
        assert_eq!(route("/who", None, "lark-1", false, &inbox).0, Route::Who);
        assert_eq!(
            route("/clear", None, "lark-1", false, &inbox).0,
            Route::Clear
        );
        assert_eq!(
            route("/all standup in five", None, "lark-1", false, &inbox),
            (Route::Board { asked: true }, "standup in five".into())
        );
        // A mistyped command is answered rather than delivered. Sending it on
        // as text is the worst of both: the person believes they aimed this
        // chat, and every sentence after it lands somewhere they did not mean.
        assert_eq!(
            route("/slect lexer", None, "lark-1", false, &inbox).0,
            Route::Unknown("/slect".into())
        );
        assert_eq!(
            route("/SELECT lexer", None, "lark-1", false, &inbox).0,
            Route::Aim("lexer".into()),
            "the commands are not case sensitive; a phone capitalises for you"
        );
        // A path is not a command, however it starts. Somebody saying where a
        // file is must not be answered with a list of commands.
        for path in ["/etc/hosts is wrong", "/usr/bin/env python", "/x.y broke"] {
            assert!(
                matches!(
                    route(path, None, "lark-1", false, &inbox).0,
                    Route::Board { asked: false }
                ),
                "{path} is a path somebody is talking about"
            );
        }

        // A reply reaches whoever wrote the card, without anyone having aimed
        // anything: this is the shortest way to answer, and the one a person
        // finds without being told.
        inbox.remember(ChannelReceipt {
            channel: "lark-1".into(),
            message_id: "om_1".into(),
            machine: String::new(),
            session_id: "s-lexer".into(),
            label: "lexer".into(),
        });
        let (decision, text) = route("yes, go ahead", Some("om_1"), "lark-1", false, &inbox);
        assert_eq!(text, "yes, go ahead");
        match decision {
            Route::Agent(who) => assert_eq!(who.session_id, "s-lexer"),
            other => panic!("a reply must answer its card: {other:?}"),
        }
        // But a command typed as a reply is still a command.
        assert_eq!(
            route("/who", Some("om_1"), "lark-1", false, &inbox).0,
            Route::Who
        );

        // Once aimed, a plain sentence goes to the same agent every time, and
        // per chat: two chats can be talking to two different agents.
        inbox
            .aimed
            .insert("lark-2".into(), who("s-parser", "parser"));
        assert_eq!(
            route("keep going", None, "lark-2", false, &inbox).0,
            Route::Agent(who("s-parser", "parser"))
        );
        assert_eq!(
            route("keep going", None, "lark-1", false, &inbox).0,
            Route::Board { asked: false }
        );
    }

    #[test]
    fn in_a_chat_with_one_person_in_it_yes_means_yes_to_whoever_asked() {
        let mut inbox = Inbox::default();
        inbox.remember(ChannelReceipt {
            channel: "wechat-1".into(),
            message_id: "m_1".into(),
            machine: String::new(),
            session_id: "s-lexer".into(),
            label: "lexer".into(),
        });
        // Nobody replied to anything and nobody aimed anything. In a group that
        // is not enough to guess from; in a chat with one person and one agent
        // in it, it is the only thing it can mean.
        assert_eq!(
            route("go ahead", None, "wechat-1", true, &inbox).0,
            Route::Agent(Correspondent {
                machine: String::new(),
                session_id: "s-lexer".into(),
                label: "lexer".into(),
                ..Default::default()
            })
        );
        assert_eq!(
            route("go ahead", None, "wechat-1", false, &inbox).0,
            Route::Board { asked: false },
            "the same sentence in a group is nobody's in particular"
        );
        // And it is per chat: another binding's history is not this one's.
        assert_eq!(
            route("go ahead", None, "wechat-2", true, &inbox).0,
            Route::Board { asked: false }
        );
    }

    /// Who the last card came from beats where the chat was aimed. A person
    /// reading their phone answers what is in front of them, and an aim set
    /// before that card is not what they are looking at.
    #[test]
    fn an_unaddressed_answer_goes_to_whoever_wrote_last_not_to_an_older_aim() {
        let mut inbox = Inbox::default();
        inbox.aim("wechat-1", who("s-parser", "parser"));
        // Nothing has been said since, so the aim is also the last word.
        assert_eq!(
            route("go ahead", None, "wechat-1", true, &inbox).0,
            Route::Agent(who("s-parser", "parser")),
            "aiming must put its agent at the end of the line, or /select does nothing"
        );

        inbox.remember(ChannelReceipt {
            channel: "wechat-1".into(),
            message_id: "m_9".into(),
            machine: "seed".into(),
            session_id: "s-lexer".into(),
            label: "lexer".into(),
        });
        assert_eq!(
            route("go ahead", None, "wechat-1", true, &inbox).0,
            Route::Agent(who("s-lexer", "lexer")),
            "the card that just arrived is what an unaddressed yes answers"
        );
        // A quote is still an address, and outranks both.
        assert_eq!(
            route("go ahead", Some("m_9"), "wechat-1", true, &inbox).0,
            Route::Agent(who("s-lexer", "lexer"))
        );
        // In a group nothing is anybody's in particular, so there the aim is
        // the only thing that settles it.
        assert_eq!(
            route("go ahead", None, "wechat-1", false, &inbox).0,
            Route::Agent(who("s-parser", "parser"))
        );

        // Clearing takes back the aim and the receipt aiming filed, so the
        // last real card is once again the last word.
        inbox.unaim("wechat-1");
        assert_eq!(
            route("go ahead", None, "wechat-1", false, &inbox).0,
            Route::Board { asked: false }
        );
        assert_eq!(
            route("go ahead", None, "wechat-1", true, &inbox).0,
            Route::Agent(who("s-lexer", "lexer"))
        );

        // An agent that has exited is forgotten entirely: aim and receipts.
        // Otherwise every following sentence chases the same dead session.
        inbox.aim("wechat-1", who("s-lexer", "lexer"));
        inbox.forget("wechat-1", "s-lexer");
        assert_eq!(
            route("go ahead", None, "wechat-1", true, &inbox).0,
            Route::Board { asked: false }
        );
    }

    #[test]
    fn a_wechat_quote_finds_the_agent_whose_message_it_named() {
        let mut inbox = Inbox::default();
        // The id is exactly what the send's reply body named and the quote
        // arrived naming: a number, as a string, on both ends.
        inbox.remember(ChannelReceipt {
            channel: "wechat-1".into(),
            message_id: "7498971037873973384".into(),
            machine: "seed".into(),
            session_id: "s-lexer".into(),
            label: "lexer".into(),
        });
        // Even in a group, even unaimed: the quote is an address, and it is
        // the shortest way back to the agent that wrote the quoted message.
        assert_eq!(
            route(
                "你好你好",
                Some("7498971037873973384"),
                "wechat-1",
                false,
                &inbox
            )
            .0,
            Route::Agent(who("s-lexer", "lexer"))
        );
        // A quote naming a message nobody here sent matches nothing, and
        // falls through exactly as an unquoted sentence would.
        assert_eq!(
            route("hi", Some("7498971037873973999"), "wechat-1", false, &inbox).0,
            Route::Board { asked: false }
        );
    }

    #[test]
    fn an_answer_to_a_quote_of_our_own_quotes_their_message_back() {
        let mut inbox = Inbox::default();
        inbox.remember(ChannelReceipt {
            channel: "wechat-1".into(),
            message_id: "7498971037873973384".into(),
            machine: "seed".into(),
            session_id: "s-lexer".into(),
            label: "lexer".into(),
        });
        // The captured exchange: they quoted our 7498971037873973384, and
        // their own message is 7498971246643971720. The answer points at
        // theirs — the phone draws their quote inside it, so that one link
        // carries the whole thread.
        let quoting = Incoming {
            message_id: "7498971246643971720".into(),
            reply_to: Some("7498971037873973384".into()),
            at: 1,
            text: "你好你好".into(),
            context_token: "ctx".into(),
            stale: false,
        };
        assert_eq!(
            quoting_target(&quoting, &inbox).as_deref(),
            Some("7498971246643971720")
        );

        // A plain message quotes nothing back, however well-known its author
        // is: the answer to it is a new message, not a thread.
        let plain = Incoming {
            reply_to: None,
            ..quoting.clone()
        };
        assert_eq!(quoting_target(&plain, &inbox), None);
        // A quote of a message we never sent is nobody's thread of ours.
        let foreign = Incoming {
            reply_to: Some("7498971037873973000".into()),
            ..quoting.clone()
        };
        assert_eq!(quoting_target(&foreign, &inbox), None);
        // A message WeChat never numbered has a synthetic catch-up id, and
        // the platform cannot point a quote at something it never named.
        let unnumbered = Incoming {
            message_id: "wechat-wechat-1-1787894069221".into(),
            ..quoting
        };
        assert_eq!(quoting_target(&unnumbered, &inbox), None);
    }

    #[test]
    fn a_message_nobody_was_listening_for_still_gives_up_the_token_that_answers_it() {
        let binding = wechat("wechat-1");
        let mut inbox = Inbox::default();
        let mut round = InboxRound::default();
        let hello = Incoming {
            message_id: "m_1".into(),
            reply_to: None,
            at: 1_787_659_402_199,
            text: "hello".into(),
            context_token: "ctx-fresh".into(),
            // The catch-up round a chat nothing had read yet comes back with.
            stale: true,
        };

        // Not delivered — nobody here was around to be told — and yet the whole
        // point of reading it is kept, because a ClawBot with no token is a bot
        // that cannot say a word.
        assert!(absorb(&hello, &binding, &mut inbox, &mut round).is_none());
        assert_eq!(
            round.refreshed,
            vec![("wechat-1".to_string(), "ctx-fresh".to_string())]
        );
        assert_eq!(round.read, 0);
        assert_eq!(inbox.seen.get("wechat-1"), Some(&1_787_659_402_199));

        // The next thing they say is theirs to answer, through a binding
        // holding the token that came with it rather than the one on file.
        let asked = Incoming {
            message_id: "m_2".into(),
            context_token: "ctx-newer".into(),
            stale: false,
            ..hello.clone()
        };
        let answering =
            absorb(&asked, &binding, &mut inbox, &mut round).expect("that one is for somebody");
        assert_eq!(answering.context_token, "ctx-newer");
        assert_eq!(answering.secret, binding.secret, "same bot, newer token");

        // And an overlapping window handing the same message back does not
        // deliver it twice.
        assert!(absorb(&asked, &binding, &mut inbox, &mut round).is_none());
    }

    #[test]
    fn a_reply_to_a_card_nobody_kept_lands_where_the_chat_is_pointed() {
        let mut inbox = Inbox {
            aimed: HashMap::from([("lark-1".to_string(), who("s-parser", "parser"))]),
            ..Default::default()
        };
        // The table is bounded, so a long conversation eventually forgets its
        // own beginning. What must not happen then is silence: the aim is the
        // fallback, and the board is the fallback after that.
        for number in 0..RECEIPT_CAP + 5 {
            inbox.remember(ChannelReceipt {
                channel: "lark-1".into(),
                message_id: format!("om_{number}"),
                machine: String::new(),
                session_id: "s-lexer".into(),
                label: "lexer".into(),
            });
        }
        assert_eq!(inbox.receipts.len(), RECEIPT_CAP);
        assert!(inbox.sender_of("om_0").is_none(), "the oldest is forgotten");
        assert!(inbox.sender_of("om_260").is_some(), "the newest is kept");
        assert_eq!(
            route("thanks", Some("om_0"), "lark-1", false, &inbox).0,
            Route::Agent(who("s-parser", "parser"))
        );

        // And a message already acted on is never acted on twice, however many
        // times an overlapping window hands it back.
        assert!(!inbox.handled.contains(&"om_9".to_string()));
        inbox.mark("om_9");
        assert!(inbox.handled.contains(&"om_9".to_string()));
    }

    #[test]
    fn a_select_word_finds_a_session_by_its_name_its_id_or_its_machine() {
        let session = Correspondent {
            machine: "seed-debug".into(),
            session_id: "a7f3c1".into(),
            label: "lexer rewrite".into(),
            ..Default::default()
        };
        for needle in ["lexer", "LEXER", "a7f3", "seed/lexer", "seed-debug/a7f3"] {
            assert!(session.answers_to(needle), "{needle} names it");
        }
        for needle in ["parser", "other/lexer", "b7f3"] {
            assert!(!session.answers_to(needle), "{needle} does not name it");
        }
        // A session with no label is still worth reading back: half an id
        // tells two of them apart, where nothing at all does not.
        assert_eq!(session.name(), "lexer rewrite · seed-debug");
    }

    #[test]
    fn only_what_a_person_typed_comes_back_out_of_a_lark_message() {
        let text =
            |kind: &str, content: &str| json!({ "msg_type": kind, "body": { "content": content } });
        // Reaching the bot in a group means @-ing it, and the @ arrives as a
        // placeholder nobody typed and nobody wants delivered.
        assert_eq!(
            lark_text(&text("text", r#"{"text":"@_user_1 ship it"}"#)),
            Some("ship it".into())
        );
        // A rich post is paragraphs of runs; the readable part is the text.
        assert_eq!(
            lark_text(&text(
                "post",
                r#"{"title":"t","content":[[{"tag":"text","text":"one"}],
                   [{"tag":"a","href":"h","text":"two"}]]}"#
            )),
            Some("one two".into())
        );
        // An image or a file has nothing to route, and an @ on its own is a
        // person getting the bot's attention, not a message for an agent.
        assert_eq!(lark_text(&text("image", r#"{"image_key":"k"}"#)), None);
        assert_eq!(lark_text(&text("text", r#"{"text":"@_user_1"}"#)), None);
    }

    #[test]
    fn help_is_its_own_command_and_lists_the_surface() {
        let inbox = Inbox::default();
        assert_eq!(route("/help", None, "lark-1", false, &inbox).0, Route::Help);
        assert_eq!(route("/h", None, "lark-1", false, &inbox).0, Route::Help);
        assert_eq!(
            route("/help", Some("om_1"), "lark-1", false, &inbox).0,
            Route::Help,
            "a command is a command even when typed as a reply"
        );
    }

    #[test]
    fn the_new_wire_commands_map_where_they_should() {
        let inbox = Inbox::default();
        // The run-a-fresh-agent and switch-default-kind commands, plus the two
        // small inspection/stop ones, each read as their own route. The words
        // carried after /call and /agent are preserved.
        assert_eq!(
            route("/call go check the logs", None, "lark-1", false, &inbox),
            (
                Route::Call("go check the logs".into()),
                "go check the logs".into()
            )
        );
        assert_eq!(
            route("/agent pi", None, "lark-1", false, &inbox).0,
            Route::AgentKind("pi".into())
        );
        // cc-connect spells the same thing /model; we accept it too.
        assert_eq!(
            route("/model codex", None, "lark-1", false, &inbox).0,
            Route::AgentKind("codex".into())
        );
        assert_eq!(
            route("/current", None, "lark-1", false, &inbox).0,
            Route::Current
        );
        assert_eq!(route("/stop", None, "lark-1", false, &inbox).0, Route::Stop);
        // /new carries the machine it was given, including nothing at all —
        // which machine is asked for rather than guessed, so the empty case has
        // to reach the handler to be answered there.
        assert_eq!(
            route("/new 2", None, "lark-1", false, &inbox).0,
            Route::New("2".into())
        );
        assert_eq!(
            route("/new", None, "lark-1", false, &inbox).0,
            Route::New(String::new())
        );
        // parse_kind is case-insensitive and rejects what is not a runtime.
        assert_eq!(parse_kind("CLAUDE"), Some(AgentKind::Claude));
        assert_eq!(parse_kind("pi"), Some(AgentKind::Pi));
        assert_eq!(parse_kind("opencode"), Some(AgentKind::OpenCode));
        assert_eq!(parse_kind("grok"), None);
        assert_eq!(parse_kind(""), None);
    }

    /// A chat message names an agent; it never prints its id. The id is exact
    /// and says nothing, and `muxloomd-claude-1787996682-39374-2` on a phone is
    /// something the person has to decode rather than read.
    #[test]
    fn an_agent_is_named_to_the_human_and_never_numbered() {
        let named = Correspondent {
            machine: "seed".into(),
            session_id: "muxloomd-claude-1787996682-39374-2".into(),
            label: "lexer".into(),
            ..Default::default()
        };
        assert_eq!(named.name(), "lexer · seed");
        // Nameless, but it has been doing something: the recap says which agent
        // this is far better than the id would.
        let working = Correspondent {
            recap: Some("rewriting the tokenizer\nand its tests".into()),
            label: String::new(),
            ..named.clone()
        };
        assert_eq!(working.name(), "rewriting the tokenizer · seed");
        // Nameless and silent: the session's number, which is at least short
        // enough to tell two apart, and never the whole id.
        let silent = Correspondent {
            recap: None,
            label: String::new(),
            machine: String::new(),
            ..named.clone()
        };
        assert_eq!(silent.name(), "2");
    }

    /// `/select <name>` and `/who` ran over every session the machines
    /// remembered, the finished ones included, while `/list` and
    /// `/select <machine>-<n>` showed and counted only the live ones. So a
    /// name that fitted an agent still working *and* the conversation it
    /// replaced came back as ambiguous, or aimed the chat at the dead one -
    /// and `/who`, the very list that is supposed to say who there is to talk
    /// to, was mostly sessions there is no talking to.
    #[test]
    fn a_name_aims_at_a_running_agent_and_a_finished_one_says_so() {
        let config = crate::config::Config::default();
        let runtime = Runtime::new(&config);
        let mut desk = Desk::new(&runtime, &[], &config);
        let session = |id: &str, label: &str, alive: bool| Correspondent {
            machine: "local".into(),
            session_id: id.into(),
            label: label.into(),
            path: "/works/arena".into(),
            alive,
            working: alive,
            needs_attention: false,
            recap: None,
            parent: None,
        };
        desk.sessions = Some(vec![
            session("s-old", "arena", false),
            session("s-live", "arena runner", true),
            session("s-gone", "seo", false),
        ]);
        let binding = ChannelBinding {
            id: "wechat-1".into(),
            kind: ChannelKind::WeChat,
            ..Default::default()
        };
        let mut inbox = Inbox::default();
        let said = |text: &str| Incoming {
            text: text.into(),
            ..Default::default()
        };

        // `arena` fits both the live agent and the finished one it followed.
        // There is only one of them to aim at, so it is not a choice to put
        // back to the human.
        let answer = handle(&mut desk, &binding, &said("/select arena"), &mut inbox);
        assert_eq!(answer, "· aimed at arena runner · local");
        assert_eq!(
            inbox
                .aimed
                .get("wechat-1")
                .map(|who| who.session_id.clone()),
            Some("s-live".into())
        );

        // A name only a finished session answers to is told plainly, rather
        // than aiming the chat at something that will never answer.
        let answer = handle(&mut desk, &binding, &said("/select seo"), &mut inbox);
        assert!(answer.contains("has finished"), "{answer}");
        assert_eq!(
            inbox
                .aimed
                .get("wechat-1")
                .map(|who| who.session_id.clone()),
            Some("s-live".into()),
            "a name that fits nobody live leaves the aim where it was"
        );

        // And `/who` is the running agents with what each is doing, plus a
        // count of the rest so nothing is dropped in silence.
        let roster = handle(&mut desk, &binding, &said("/who"), &mut inbox);
        assert!(
            roster.contains("- arena runner · local · working"),
            "{roster}"
        );
        assert!(!roster.contains("seo"), "{roster}");
        assert!(roster.contains("(2 finished, not listed)"), "{roster}");
    }

    /// The number `/list` prints and the number `/select` counts along were
    /// two different lists: the screen showed the active agents grouped by
    /// folder, while the command counted every session the machine had ever
    /// held, dead ones included, in the order the daemon happened to list
    /// them. On a machine with a long history that aims the chat at a
    /// stranger - typing the number next to the agent you are reading about
    /// hands your message to whoever sits at that offset among the dead.
    #[test]
    fn the_number_beside_an_agent_is_the_number_that_aims_the_chat_at_it() {
        let config = crate::config::Config::default();
        let runtime = Runtime::new(&config);
        let mut desk = Desk::new(&runtime, &[], &config);
        let session = |id: &str, label: &str, path: &str, alive: bool| Correspondent {
            machine: "local".into(),
            session_id: id.into(),
            label: label.into(),
            path: path.into(),
            alive,
            working: false,
            needs_attention: false,
            recap: None,
            parent: None,
        };
        // As a real machine reports itself: piles of finished conversations,
        // a handful of live ones, in no particular order.
        desk.sessions = Some(vec![
            session("s-dead-1", "last week", "/works/arena", false),
            session(
                "s-me",
                "the one you are talking to",
                "/works/terminal",
                true,
            ),
            session("s-dead-2", "yesterday", "/works/arena", false),
            session("s-arena", "the arena", "/works/arena", true),
            session("s-dead-3", "this morning", "/works/terminal", false),
            session("s-sibling", "its neighbour", "/works/arena", true),
        ]);

        let listed = desk.listed_agents("local");
        let refs: Vec<&Correspondent> = listed.iter().collect();
        let printed = folder_grouped(1, &refs).join("\n");
        assert!(
            !printed.contains("yesterday") && !printed.contains("last week"),
            "only the live ones are printed: {printed}"
        );
        // Whatever number stands next to an agent on the screen, typing that
        // number reaches that agent - checked for every line of the list.
        for (index, who) in listed.iter().enumerate() {
            let number = index + 1;
            assert!(
                printed.contains(&format!("  1-{number}  {}", who.list_name())),
                "agent 1-{number} is printed as {}: {printed}",
                who.list_name()
            );
            assert_eq!(
                desk.resolve_numbered(1, number).map(|it| it.session_id),
                Some(who.session_id.clone()),
                "`/select 1-{number}` must reach the agent printed as {number}"
            );
        }
        // Which, in the case that started this, is the arena agent rather than
        // the second session the machine happened to list.
        assert_eq!(
            desk.resolve_numbered_from("1-1").map(|it| it.session_id),
            Some("s-arena".into())
        );
        assert_eq!(desk.resolve_numbered(1, listed.len() + 1), None);
    }

    /// A phone is not a dashboard. One agent handing its work out five ways is
    /// an ordinary morning, and every one of those subagents used to arrive in
    /// the chat list beside the agent that started them — so the list the human
    /// reads to find their own agent was mostly agents they had never heard of,
    /// and the numbers beside it moved every time somebody fanned out.
    #[test]
    fn the_chat_lists_the_agents_a_person_started_and_not_the_ones_agents_did() {
        let config = crate::config::Config::default();
        let runtime = Runtime::new(&config);
        let mut desk = Desk::new(&runtime, &[], &config);
        let session = |id: &str, label: &str, parent: Option<&str>| Correspondent {
            machine: "local".into(),
            session_id: id.into(),
            label: label.into(),
            path: "/works/arena".into(),
            alive: true,
            working: false,
            needs_attention: false,
            recap: None,
            parent: parent.map(str::to_string),
        };
        desk.sessions = Some(vec![
            session("s-lead", "the arena", None),
            session("s-child-1", "reviewing the parser", Some("s-lead")),
            session("s-child-2", "reviewing the lexer", Some("s-lead")),
            session("s-other", "the seo run", None),
        ]);

        let listed = desk.listed_agents("local");
        assert_eq!(
            listed
                .iter()
                .map(|it| it.session_id.as_str())
                .collect::<Vec<_>>(),
            ["s-lead", "s-other"],
            "only the agents a person started belong in a chat list"
        );
        let refs: Vec<&Correspondent> = listed.iter().collect();
        let printed = folder_grouped(1, &refs).join("\n");
        assert!(
            !printed.contains("reviewing the"),
            "a subagent is not printed: {printed}"
        );
        // And the numbers count along that same list, so `/select 1-2` is the
        // second agent printed rather than the second session on the machine.
        assert!(printed.contains("  1-2  the seo run"), "{printed}");
        assert_eq!(
            desk.resolve_numbered_from("1-2").map(|it| it.session_id),
            Some("s-other".into())
        );
        // `/who` is the same roster read a different way, so it counts the
        // same agents - and says how many are working under them, because
        // dropping them in silence would read as nobody being on it.
        let binding = ChannelBinding {
            id: "wechat-1".into(),
            kind: ChannelKind::WeChat,
            ..Default::default()
        };
        let mut inbox = Inbox::default();
        let roster = handle(
            &mut desk,
            &binding,
            &Incoming {
                text: "/who".into(),
                ..Default::default()
            },
            &mut inbox,
        );
        assert!(roster.contains("- the arena · local"), "{roster}");
        assert!(roster.contains("- the seo run · local"), "{roster}");
        assert!(!roster.contains("reviewing the"), "{roster}");
        assert!(roster.contains("(2 working under them)"), "{roster}");

        // `/list` counts the same agents beside each machine, so a person can
        // see where the work is without opening every machine to find out.
        let listing = handle(
            &mut desk,
            &binding,
            &Incoming {
                text: "/list".into(),
                ..Default::default()
            },
            &mut inbox,
        );
        assert!(listing.contains("· 2 agents"), "{listing}");

        // And one machine's list says outright that the rest are working
        // under the ones it named, rather than dropping them in silence.
        let opened = handle(
            &mut desk,
            &binding,
            &Incoming {
                text: "/list 1".into(),
                ..Default::default()
            },
            &mut inbox,
        );
        assert!(opened.contains("  1-1  the arena"), "{opened}");
        assert!(opened.contains("  1-2  the seo run"), "{opened}");
        assert!(!opened.contains("reviewing the"), "{opened}");
        assert!(
            opened.contains("2 more working under the ones listed"),
            "{opened}"
        );

        // Nothing is hidden by leaving them out: a chat already aimed at a
        // subagent still finds it, because that goes by id and not by number.
        let aimed = Correspondent {
            machine: String::new(),
            ..session("s-child-1", "reviewing the parser", Some("s-lead"))
        };
        assert_eq!(
            desk.locate(&aimed).map(|it| it.session_id),
            Some("s-child-1".into())
        );
    }

    #[test]
    fn list_groups_active_agents_by_folder_and_carries_their_recap() {
        // Two folders, a working agent and an idle one, and a dead one that
        // must not show up at all.
        let agents = [
            Correspondent {
                machine: "m".into(),
                session_id: "s-1".into(),
                label: "lexer".into(),
                path: "/works/x/".into(),
                alive: true,
                working: true,
                needs_attention: false,
                recap: Some("splitting the lexer".into()),
                parent: None,
            },
            Correspondent {
                machine: "m".into(),
                session_id: "s-2".into(),
                label: "parser".into(),
                path: "/works/x/".into(),
                alive: true,
                working: false,
                needs_attention: false,
                recap: None,
                parent: None,
            },
            Correspondent {
                machine: "m".into(),
                session_id: "s-dead".into(),
                label: "dead".into(),
                path: "/works/x/".into(),
                alive: false,
                working: false,
                needs_attention: false,
                recap: None,
                parent: None,
            },
        ];
        // Route's job is only to say "a /list" — the active filter and the
        // grouping are the handler's. So we exercise the pure renderer here
        // over exactly the active subset the handler would pass it.
        let active: Vec<&Correspondent> = agents.iter().filter(|a| a.alive).collect();
        let lines = folder_grouped(1, &active);
        let rendered = lines.join("\n");
        assert!(rendered.contains("/works/x"), "folder is the heading");
        assert!(rendered.contains("lexer"), "the labelled agent is there");
        assert!(
            rendered.contains("splitting the lexer"),
            "its recap follows"
        );
        assert!(
            !rendered.contains("dead"),
            "an inactive agent is not grouped in"
        );
        // Numbering stays stable across folders, so /select's machine-agent
        // numbers stay stable even as folders move around.
        assert!(
            rendered.contains("  1-1  lexer · working"),
            "numbering starts at one: {rendered}"
        );
    }

    /// A folder heading is read on a phone, where a full path is a line and a
    /// half of which most is the same prefix on every row — and the part of it
    /// that is somebody's home directory has no business in a chat at all.
    #[test]
    fn a_folder_heading_is_the_end_of_the_path_rather_than_the_whole_of_it() {
        assert_eq!(
            folder_shown("/Users/someone/Works/Terminal"),
            "…/Works/Terminal"
        );
        // Nothing dropped, nothing marked as dropped.
        assert_eq!(folder_shown("/works/x/"), "/works/x");
        assert_eq!(folder_shown("/works"), "/works");
        // A session that did not say where it runs keeps its own heading.
        assert_eq!(folder_shown("~"), "~");
        // Grouping is still by the whole path, so two folders that end the
        // same way stay two folders even though they now read alike.
        let agent = |path: &str, label: &str| Correspondent {
            machine: "m".into(),
            session_id: format!("s-{label}"),
            label: label.into(),
            path: path.into(),
            alive: true,
            working: false,
            needs_attention: false,
            recap: None,
            parent: None,
        };
        let agents = [
            agent("/home/a/works/api", "one"),
            agent("/home/b/works/api", "two"),
        ];
        let listed: Vec<&Correspondent> = agents.iter().collect();
        let rendered = folder_grouped(2, &listed).join("\n");
        assert_eq!(
            rendered.matches("…/works/api").count(),
            2,
            "two headings, not one: {rendered}"
        );
        // And the number printed is the pair `/select` takes, machine and all.
        assert!(rendered.contains("  2-1  one"), "{rendered}");
        assert!(rendered.contains("  2-2  two"), "{rendered}");
    }

    /// A chat has no colour and no spinner, so an agent's line has to say
    /// outright what it is doing. Without it the only thing under a name is
    /// the recap, and a recap reads the same whether the agent said it a
    /// moment ago or hours before it stopped — which is how a row of agents
    /// nobody had touched all looked like they were running.
    #[test]
    fn a_listed_agent_says_whether_it_is_working_waiting_or_idle() {
        let agent = |label: &str, working: bool, needs_attention: bool| Correspondent {
            machine: "m".into(),
            session_id: format!("s-{label}"),
            label: label.into(),
            path: "/works/x".into(),
            alive: true,
            working,
            needs_attention,
            recap: Some("✻ Burrowing… (3h 59m 0s · ↓ 794.8k tokens)".into()),
            parent: None,
        };
        let agents = [
            agent("busy", true, false),
            agent("asking", false, true),
            agent("done", false, false),
        ];
        let listed: Vec<&Correspondent> = agents.iter().collect();
        let rendered = folder_grouped(1, &listed).join("\n");
        assert!(rendered.contains("  1-1  busy · working"), "{rendered}");
        assert!(
            rendered.contains("  1-2  asking · waiting for you"),
            "{rendered}"
        );
        assert!(rendered.contains("  1-3  done · idle"), "{rendered}");

        // A session that has stopped for good says so rather than keeping
        // whatever it last happened to be.
        let finished = Correspondent {
            alive: false,
            working: true,
            ..agents[0].clone()
        };
        assert_eq!(finished.state(), "finished");
    }

    #[test]
    fn approval_replies_are_recognised_and_reach_a_verdict() {
        let inbox = Inbox::default();
        match route("approve-12", None, "lark-1", false, &inbox).0 {
            Route::Approval { id, verdict } => {
                assert_eq!(id, "approve-12");
                assert_eq!(verdict, crate::approvals::Verdict::Yes);
            }
            other => panic!("approve-12 should route to an approval: {other:?}"),
        }
        match route("always-7", None, "lark-1", false, &inbox).0 {
            Route::Approval { id, verdict } => {
                assert_eq!(id, "approve-7");
                assert_eq!(verdict, crate::approvals::Verdict::Always);
            }
            other => panic!("always-7 should route to an approval: {other:?}"),
        }
        match route("reject-3", None, "lark-1", false, &inbox).0 {
            Route::Approval { id, verdict } => {
                assert_eq!(id, "approve-3");
                assert_eq!(verdict, crate::approvals::Verdict::No);
            }
            other => panic!("reject-3 should route to an approval: {other:?}"),
        }
        // A number with no verb, or a plain sentence, is not an approval.
        assert!(matches!(
            route("123", None, "lark-1", false, &inbox).0,
            Route::Board { .. }
        ));
        assert!(matches!(
            route("approve", None, "lark-1", false, &inbox).0,
            Route::Board { .. }
        ));
    }

    fn lease_note(holder: &str, _account: &str, until: u64, cursor: &str, ts: u64) -> Leased {
        Leased {
            holder: holder.into(),
            until,
            cursor: cursor.into(),
            ts,
        }
    }

    #[test]
    fn lease_election_yields_to_a_smaller_live_holder_and_takes_over_after_expiry() {
        // Connected fleet, two machines: the smaller origin holds a live lease,
        // so this machine stays silent for the account.
        assert_eq!(
            lease_decision(
                1_000,
                "machine-b",
                &[lease_note(
                    "machine-a",
                    "bot@im",
                    1_000 + LEASE_TTL_MS,
                    "cur-a",
                    1_000
                )],
                0,
                true,
            ),
            LeaseDecision::Yield
        );
        // The holder's lease lapses (its controller died) and nobody else holds
        // the account: this machine enters the election — whatever it has done
        // before, the board is empty of live leases, so it introduces and stays
        // quiet for the settle window.
        assert_eq!(
            lease_decision(
                1_000 + LEASE_TTL_MS + 1,
                "machine-b",
                &[lease_note(
                    "machine-a",
                    "bot@im",
                    1_000 + LEASE_TTL_MS,
                    "cur-a",
                    1_000
                )],
                0,
                true,
            ),
            LeaseDecision::Introduce
        );
        // Inside its own settle window: the lease is on the board and is the
        // smallest (only) live one, but the other side may not have seen it
        // yet — keep it alive and hold one more round.
        let takeover = 1_000 + LEASE_TTL_MS + 1;
        assert_eq!(
            lease_decision(
                takeover + 5_000,
                "machine-b",
                &[lease_note(
                    "machine-b",
                    "bot@im",
                    takeover + LEASE_TTL_MS,
                    "cur-b",
                    takeover
                )],
                takeover,
                true,
            ),
            LeaseDecision::Quiet
        );
        // The settle window is over and the lease is still live: speak.
        assert_eq!(
            lease_decision(
                takeover + LEASE_SETTLE_MS,
                "machine-b",
                &[lease_note(
                    "machine-b",
                    "bot@im",
                    takeover + LEASE_TTL_MS,
                    "cur-b",
                    takeover
                )],
                takeover,
                true,
            ),
            LeaseDecision::Consume
        );
    }

    #[test]
    fn lease_election_consumes_on_its_own_live_lease_and_yields_to_a_smaller() {
        let now = 100_000u64;
        let mine = lease_note(
            "machine-a",
            "bot@im",
            now + LEASE_TTL_MS,
            "cur",
            now - 5_000,
        );
        // I hold the only live lease and my settle window is over: speak.
        assert_eq!(
            lease_decision(
                now,
                "machine-a",
                std::slice::from_ref(&mine),
                now - LEASE_SETTLE_MS,
                true
            ),
            LeaseDecision::Consume
        );
        // I hold the only live lease but I just introduced: stay quiet.
        assert_eq!(
            lease_decision(
                now,
                "machine-a",
                std::slice::from_ref(&mine),
                now - 1_000,
                true
            ),
            LeaseDecision::Quiet
        );
        // A smaller holder appearing with a live lease means I lose: yield.
        assert_eq!(
            lease_decision(
                now,
                "machine-a",
                &[
                    mine,
                    lease_note("machine-0", "bot@im", now + LEASE_TTL_MS, "cur0", now),
                ],
                now - LEASE_SETTLE_MS,
                true,
            ),
            LeaseDecision::Yield
        );
    }

    #[test]
    fn lease_election_without_live_peers_always_consumes() {
        let now = 100_000u64;
        // 断开: the board may still hold a fresh lease from a machine that is
        // no longer reachable — honoring it would silence this side of the
        // split, which is the failover case. Consume.
        assert_eq!(
            lease_decision(
                now,
                "machine-a",
                &[lease_note(
                    "machine-b",
                    "bot@im",
                    now + LEASE_TTL_MS,
                    "cur-b",
                    now - 1_000
                )],
                0,
                false,
            ),
            LeaseDecision::Consume
        );
        // No peers at all: speak immediately, and there is no order to settle.
        assert_eq!(
            lease_decision(now, "machine-a", &[], 0, false),
            LeaseDecision::Consume
        );
    }

    #[test]
    fn lease_election_first_contact_introduces_with_a_live_peer() {
        // Cold start on both sides: empty board, live peer. Post the lease, but
        // stay quiet for the settle window so a simultaneous first claim on the
        // other side cannot double-answer the batch.
        assert_eq!(
            lease_decision(1_000, "machine-a", &[], 0, true),
            LeaseDecision::Introduce
        );
        // A machine that claimed long ago meets the same empty board (its note
        // was lost and so was the other side's): it still introduces, because
        // with a live peer and no lease on the board nobody's word is current.
        assert_eq!(
            lease_decision(1_000, "machine-a", &[], 900_000, true),
            LeaseDecision::Introduce
        );
        // The same situation with no live peer: no race possible, speak now.
        assert_eq!(
            lease_decision(1_000, "machine-a", &[], 0, false),
            LeaseDecision::Consume
        );
    }

    #[test]
    fn a_successor_resumes_at_the_newest_foreign_cursor() {
        let mut inbox = Inbox::default();
        let binding = ChannelBinding {
            id: "wechat-1".into(),
            app_id: "a1beaee1847a@im.bot".into(),
            ..Default::default()
        };
        inbox.cursors.insert("wechat-1".into(), "my-stale".into());
        // Someone else's newest word on the account, with a consumed cursor:
        // resume there, past what the dead voice already read.
        let foreign = lease_note("machine-a", "bot@im", 2_000, "their-cursor", 1_000);
        adopt_newer_cursor(
            &mut inbox,
            &binding,
            std::slice::from_ref(&foreign),
            "machine-b",
        );
        assert_eq!(
            inbox.cursors.get("wechat-1").map(String::as_str),
            Some("their-cursor")
        );
        // A note I wrote myself is never newer than my live cursor: skip.
        let mine = lease_note("machine-b", "bot@im", 2_000, "my-old-note", 1_500);
        adopt_newer_cursor(
            &mut inbox,
            &binding,
            std::slice::from_ref(&mine),
            "machine-b",
        );
        assert_eq!(
            inbox.cursors.get("wechat-1").map(String::as_str),
            Some("their-cursor")
        );
        // A newer foreign note with an empty cursor consumed nothing: keep my
        // own position rather than walking backwards.
        let empty = lease_note("machine-a", "bot@im", 2_000, "", 2_000);
        adopt_newer_cursor(
            &mut inbox,
            &binding,
            std::slice::from_ref(&empty),
            "machine-b",
        );
        assert_eq!(
            inbox.cursors.get("wechat-1").map(String::as_str),
            Some("their-cursor")
        );
        // With no position of my own there is nothing to walk backwards to —
        // the empty cursor is the same as none, and the first-round catch-up
        // applies.
        let mut bare = Inbox::default();
        adopt_newer_cursor(
            &mut bare,
            &binding,
            std::slice::from_ref(&empty),
            "machine-b",
        );
        assert!(!bare.cursors.contains_key("wechat-1"));
        adopt_newer_cursor(
            &mut bare,
            &binding,
            std::slice::from_ref(&foreign),
            "machine-b",
        );
        assert_eq!(
            bare.cursors.get("wechat-1").map(String::as_str),
            Some("their-cursor")
        );
    }

    #[test]
    fn lease_notes_parse_only_their_own_shape() {
        fn note(origin: &str, ts: u64, text: &str) -> crate::talk::TalkMessage {
            crate::talk::TalkMessage {
                id: format!("{origin}:1"),
                origin: origin.into(),
                seq: 1,
                ts,
                scope: crate::talk::TalkScope::Path {
                    machine: origin.into(),
                    path: LEASE_PATH.into(),
                },
                author: Default::default(),
                kind: crate::talk::TalkKind::Note,
                to: None,
                reply_to: None,
                text: text.into(),
            }
        }
        let good = note(
            "machine-a",
            42,
            &format!("{LEASE_PREFIX}\naccount=bot@im\nuntil=999\ncursor=abc123"),
        );
        assert_eq!(
            lease_from(&good, "bot@im"),
            Some(Leased {
                holder: "machine-a".into(),
                until: 999,
                cursor: "abc123".into(),
                ts: 42,
            })
        );
        // Another account's lease, a plain note, an old single-line note, and a
        // note missing its cursor are all simply not a lease for this account.
        assert_eq!(lease_from(&good, "other@im"), None);
        assert_eq!(
            lease_from(&note("machine-b", 1, "plain board note"), "bot@im"),
            None
        );
        assert_eq!(
            lease_from(
                &note(
                    "machine-b",
                    1,
                    &format!("{LEASE_PREFIX}account=bot@im until=999")
                ),
                "bot@im"
            ),
            None
        );
        assert_eq!(
            lease_from(
                &note(
                    "machine-b",
                    1,
                    &format!("{LEASE_PREFIX}\naccount=bot@im\nuntil=999")
                ),
                "bot@im"
            ),
            None
        );
    }

    #[test]
    fn account_key_uses_the_bot_identity_not_the_local_name() {
        let binding = ChannelBinding {
            id: "wechat-1".into(),
            app_id: "  a1beaee1847a@im.bot ".into(),
            route: "user-123".into(),
            ..Default::default()
        };
        assert_eq!(account_key(&binding), "a1beaee1847a@im.bot");
        // No bot id: fall back to the person it was scanned to.
        let bare = ChannelBinding {
            id: "wechat-2".into(),
            app_id: String::new(),
            route: "user-456".into(),
            ..Default::default()
        };
        assert_eq!(account_key(&bare), "user-456");
        // Nothing at all: the local binding id is the only key left.
        let empty = ChannelBinding {
            id: "wechat-3".into(),
            ..Default::default()
        };
        assert_eq!(account_key(&empty), "wechat-3");
    }

    #[test]
    fn reply_signatures_count_per_binding_and_survive_a_reload() {
        let mut inbox = Inbox::default();
        assert_eq!(
            inbox.reply_signature("wechat-1", "G3HMWLJP75"),
            "*— G3HMWLJP75 #1*"
        );
        assert_eq!(
            inbox.reply_signature("wechat-1", "G3HMWLJP75"),
            "*— G3HMWLJP75 #2*"
        );
        // A different binding starts its own count.
        assert_eq!(inbox.reply_signature("lark-1", "macmini"), "*— macmini #1*");
        // An unknown machine name means unsigned, as before.
        assert_eq!(inbox.reply_signature("wechat-1", "   "), "");
        // The counter survives a save/load round trip.
        let dir = std::env::temp_dir().join(format!(
            "muxloom-inbox-test-{}-{}",
            std::process::id(),
            now_ms()
        ));
        let path = dir.join("channel-inbox.json");
        inbox.save(&path).unwrap();
        let mut loaded = Inbox::load(&path);
        assert_eq!(
            loaded.reply_signature("wechat-1", "G3HMWLJP75"),
            "*— G3HMWLJP75 #3*"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn peer_liveness_follows_the_last_reach_of_each_target() {
        let local = Target::local();
        no_longer_heard(&local);
        assert!(!peers_are_live(std::slice::from_ref(&local)));
        heard_from(&local);
        assert!(peers_are_live(std::slice::from_ref(&local)));
        no_longer_heard(&local);
        assert!(!peers_are_live(std::slice::from_ref(&local)));
    }

    #[test]
    fn a_remembered_machine_that_stopped_resolving_is_not_trusted_for_a_live_session() {
        // A receipt saves the machine a session was on when it answered, but
        // the receipt is answered later and the session may have been resumed
        // under a machine id the current desk no longer routes to (a hostname
        // rename, a fleet resume, a controller that sees a different set).
        // `locate` must not hand that stale machine straight through — the old
        // behaviour did, and the routing step then claimed the agent "is on a
        // machine muxloom cannot reach" even while the session was alive
        // elsewhere. With no desk target and no live session present, the
        // resolved correspondent is gone rather than poisoned.
        let config = crate::config::Config::default();
        let runtime = Runtime::new(&config);
        let mut desk = Desk::new(&runtime, &[], &config);
        let who = Correspondent {
            machine: "seed".into(),
            session_id: "session-9".into(),
            label: "lexicon".into(),
            ..Default::default()
        };
        let resolved = desk.locate(&who);
        assert!(
            resolved.is_none() || resolved.as_ref().unwrap().machine != "seed",
            "a stale remembered machine must not survive locate unchanged \
             (it would fail target() and report 'cannot reach'): {resolved:#?}"
        );
    }
}
