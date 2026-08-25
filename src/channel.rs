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

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{debug, http, ilink, model::Target, runtime::Runtime};

/// Advertised by a daemon that can be given a channel set. A daemon without it
/// is simply left out of the round; nothing else about it changes.
pub const CHANNELS_CAPABILITY: &str = "channels-v1";

/// The file every machine keeps its copy in, relative to its state directory.
pub const CHANNELS_FILE: &str = "channels.json";

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
        let mut body = clip(text, limit.saturating_sub(signature.len() + 2));
        if !signature.is_empty() {
            body.push_str("\n\n");
            body.push_str(signature);
        }
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
    /// The id a reply can be matched against. Lark's is the platform's own,
    /// which is what a human's reply points back at. WeChat's is the one this
    /// side minted, because a reply there comes back naming something else
    /// entirely — so a WeChat chat is answered by aim and by who spoke last,
    /// not by which message was replied to.
    pub message_id: String,
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
    binding.ready()?;
    if message.text.trim().is_empty() {
        bail!("a channel message needs something to say");
    }
    match binding.kind {
        ChannelKind::Lark => send_lark(binding, message, environment),
        ChannelKind::WeChat => send_wechat(binding, message, environment),
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
    })
}

/// One chat a Lark app's bot has been added to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chat {
    pub id: String,
    pub name: String,
}

/// The chats a Lark app can already talk in, newest first.
///
/// A chat id is `oc_` and thirty-odd characters of hexadecimal, and there is
/// nowhere in the Lark client to copy one from — the way people find theirs is
/// to open a chat in a browser and read the address bar. Asking is better: the
/// app knows which chats it is in, and the answer comes back with the names
/// they are known by.
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
        &format!("{LARK_HOST}/open-apis/im/v1/chats?page_size=100&sort_type=ByCreateTimeDesc"),
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
            Some(Chat {
                id: id.to_string(),
                // A group with no name set is not an error and should not read
                // as an empty row; Lark itself shows these by their members.
                name: match name.is_empty() {
                    true => "(unnamed chat)".to_string(),
                    false => name.to_string(),
                },
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
    environment: &[(String, String)],
) -> Result<Sent> {
    let (title, body) = message.compose(WECHAT_LIMIT);
    // A WeChat message has no header of its own, so a title goes back into the
    // text it was lifted out of, on a line by itself.
    let text = match title.is_empty() {
        true => plain(&body),
        false => format!("{title}\n\n{}", plain(&body)),
    };
    let message_id = ilink::send_text(
        &wechat_account(binding),
        binding.context_token.trim(),
        &text,
        environment,
    )
    .with_context(|| format!("channel {} could not reach WeChat", binding.id))?;
    Ok(Sent {
        channel: binding.id.clone(),
        through: binding.describes(),
        message_id,
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
}

impl Correspondent {
    /// How the human reads it: the label if the session has one, and enough of
    /// the id to tell two apart if it does not.
    pub fn name(&self) -> String {
        let called = if self.label.trim().is_empty() {
            self.session_id.as_str()
        } else {
            self.label.trim()
        };
        if self.machine.is_empty() {
            return called.to_string();
        }
        format!("{called} · {}", self.machine)
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
    /// Which agent sent each message a human might reply to, newest last.
    #[serde(default)]
    pub receipts: Vec<ChannelReceipt>,
    /// Message ids already routed, newest last: reading the same window twice
    /// must not deliver twice.
    #[serde(default)]
    pub handled: Vec<String>,
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
            machine: String::new(),
            session_id: receipt.session_id.clone(),
            label: receipt.label.clone(),
        }
    }
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
    /// Say what there is to talk to.
    Who,
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
    let (word, rest) = text.split_once(char::is_whitespace).unwrap_or((text, ""));
    let rest = rest.trim().to_string();
    // A command is a command wherever it was typed, including in a reply: the
    // person who writes `/who` under a card wants the list, not to send the
    // word `/who` to an agent.
    match word.to_lowercase().as_str() {
        "/select" | "/s" => return (Route::Aim(rest.clone()), rest),
        "/who" | "/help" => return (Route::Who, rest),
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
    if let Some(who) = inbox.aimed.get(binding) {
        return (Route::Agent(who.clone()), text.to_string());
    }
    if solo && let Some(who) = inbox.last_sender(binding) {
        return (Route::Agent(who), text.to_string());
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
            Ok(None) => continue,
            Ok(Some((held, receipts))) => {
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
    /// The message it answers, when it answers one. Lark only: WeChat's reply
    /// arrives pointing at an item id that has nothing to do with the id the
    /// send gave back, so there is nothing honest to match it against.
    pub reply_to: Option<String>,
    /// Epoch milliseconds, as the platform recorded it.
    pub at: u64,
    pub text: String,
    /// WeChat: the token that has to travel on anything said back. Empty for
    /// Lark, which needs no such thing.
    pub context_token: String,
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
    // Either way, catch up silently: waking an agent with a conversation that
    // happened before it existed is worse than missing it.
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
    if first {
        return Ok(Vec::new());
    }
    Ok(round
        .said
        .into_iter()
        .map(|said| Incoming {
            // WeChat does not always number a message. The cursor is what
            // actually keeps a round from repeating itself; this only has to be
            // different from its neighbours.
            message_id: match said.message_id.is_empty() {
                true => format!("wechat-{}-{}", binding.id, said.at),
                false => said.message_id,
            },
            reply_to: None,
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
}

impl InboxRound {
    pub fn busy(&self) -> bool {
        self.read > 0 || !self.failures.is_empty() || !self.refreshed.is_empty()
    }
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
    let mut desk = Desk::new(runtime, targets);
    for binding in listening {
        if inbox
            .waking
            .get(&binding.id)
            .is_some_and(|&until| now < until)
        {
            round.asleep.push(binding.id.clone());
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
        if inbox.waking.contains_key(&binding.id) {
            round.asleep.push(binding.id.clone());
        }
        for message in said {
            let newest = inbox.seen.entry(binding.id.clone()).or_default();
            *newest = (*newest).max(message.at);
            if inbox.handled.contains(&message.message_id) {
                continue;
            }
            inbox.mark(&message.message_id);
            round.read += 1;
            // The token off this very message is the one that can answer it.
            // Anything older may already have been spent, so the reply below
            // goes out through a binding holding the newest there is.
            let answering = match message.context_token.is_empty() {
                true => binding.clone(),
                false => {
                    round
                        .refreshed
                        .push((binding.id.clone(), message.context_token.clone()));
                    ChannelBinding {
                        context_token: message.context_token.clone(),
                        ..binding.clone()
                    }
                }
            };
            let answer = handle(&mut desk, binding, &message, inbox);
            round.routed.push(format!("{}: {answer}", binding.id));
            // The answer is itself something a human can reply to, so it is
            // remembered under whoever it is about — an agent's name in a
            // receipt is a handle, not just a report.
            let receipt = Outgoing {
                title: String::new(),
                text: answer,
                signature: String::new(),
            };
            match send(&answering, &receipt, environment) {
                Ok(sent) if !sent.message_id.is_empty() => {
                    if let Some(who) = desk.last_agent.clone() {
                        inbox.remember(ChannelReceipt {
                            channel: binding.id.clone(),
                            message_id: sent.message_id,
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

/// The fleet, as one round of reading needs it: listed once, however many
/// messages ask about it, and not at all when none does.
struct Desk<'a> {
    runtime: &'a Runtime,
    machines: Vec<Target>,
    sessions: Option<Vec<Correspondent>>,
    author: Option<crate::talk::TalkAuthor>,
    /// Who the message just handled was about, so the receipt can be replied to.
    last_agent: Option<Correspondent>,
}

impl<'a> Desk<'a> {
    fn new(runtime: &'a Runtime, targets: &[Target]) -> Self {
        let local = Target::local();
        let machines = std::iter::once(local.clone())
            .chain(targets.iter().filter(|it| it.id != local.id).cloned())
            .collect();
        Self {
            runtime,
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
                            label: session.label,
                        }),
                );
            }
            found
        })
    }

    /// Which machine a session is on, when a receipt only remembered its id.
    fn locate(&mut self, who: &Correspondent) -> Option<Correspondent> {
        if !who.machine.is_empty() {
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

    /// A human speaking, as the board records them. Asked of the local board
    /// once, because what this machine is called is not something a chat knows.
    fn author(&mut self, called: &str) -> crate::talk::TalkAuthor {
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
                ..Default::default()
            },
            ..base.clone()
        }
    }
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
    let (decision, text) = route(
        &message.text,
        message.reply_to.as_deref(),
        &binding.id,
        binding.kind.solo(),
        inbox,
    );
    match decision {
        Route::Who => {
            let mut lines: Vec<String> = desk
                .sessions()
                .iter()
                .map(|who| format!("- {}", who.name()))
                .collect();
            if lines.is_empty() {
                lines.push("- nobody is running".into());
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
        Route::Clear => match inbox.aimed.remove(&binding.id) {
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
            let found: Vec<Correspondent> = desk
                .sessions()
                .iter()
                .filter(|who| who.answers_to(&words))
                .cloned()
                .collect();
            match found.as_slice() {
                [] => format!("· nothing here answers to `{words}` — `/who` lists what does"),
                [only] => {
                    let name = only.name();
                    desk.last_agent = Some(only.clone());
                    inbox.aimed.insert(binding.id.clone(), only.clone());
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
                inbox.aimed.remove(&binding.id);
                return format!(
                    "· {} is not running any more, so that went nowhere. `/who` lists what is.",
                    who.name()
                );
            };
            let Some(target) = desk.target(&who.machine).cloned() else {
                return format!("· {} is on a machine muxloom cannot reach", who.name());
            };
            let draft = crate::talk::TalkDraft {
                scope: crate::talk::TalkScope::Machine {
                    machine: String::new(),
                },
                author: desk.author(&called),
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
                author: desk.author(&called),
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
        assert_eq!(body, "42 tests passed.\n\n*— gpu-1 · claude*");

        let named = Outgoing {
            title: "Nightly".into(),
            ..message.clone()
        };
        let (title, body) = named.compose(LARK_LIMIT);
        assert_eq!(title, "Nightly");
        assert!(
            body.starts_with("# 构建完成"),
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
            body.ends_with("*— gpu-1*"),
            "the signature must survive the cut: {body}"
        );
        // A cut that landed inside one of those three-byte characters would
        // have panicked on the way out of `clip`.
        assert!(body.starts_with('あ'));
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
}
