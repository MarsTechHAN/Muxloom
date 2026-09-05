//! iLink — the bot protocol behind WeChat's ClawBot plugin.
//!
//! It is what makes personal WeChat usable here at all. Everything else muxloom
//! can talk to wants an account created on some open platform first, an id and
//! a secret copied out of a console, and a chat id found afterwards; this one
//! wants a phone, a camera, and about four seconds. Somebody scans a square,
//! taps allow, and the machine has a bot that both sends and listens.
//!
//! Plain HTTP and JSON throughout — no socket to hold open, no address on the
//! public internet to be reachable at — which is what lets a daemon on a
//! machine behind three jump hosts do this for itself.
//!
//! Two things about it shape everything above:
//!
//! * **A reply needs a context token.** Every message out carries the token off
//!   the last message in, and there is no way to invent one. A bot that has
//!   never been spoken to cannot speak first, which is why binding one ends by
//!   asking for a hello rather than at the moment the code is scanned.
//! * **A session goes to sleep.** WeChat answers `-14` once the conversation
//!   has been quiet long enough, and the way back is the person saying
//!   something, not the machine trying harder. So `-14` is reported as the
//!   plain fact it is instead of being retried.

use std::path::Path;

use anyhow::{Result, anyhow, bail};
use serde_json::{Value, json};

use crate::http;

/// Where the protocol lives before a login says otherwise. A confirmed login
/// hands back the host to use from then on, which may be a nearer one.
pub const HOST: &str = "https://ilinkai.weixin.qq.com";

/// Which flavour of bot the QR asks for. Three is what WeChat's own plugin
/// sends, and the only value known to produce a scannable code.
const BOT_TYPE: &str = "3";

/// A text item, in the protocol's numbering.
const ITEM_TEXT: i64 = 1;
/// The item types that are not words. Pictures and documents muxloom both
/// sends and receives, and a video it receives; a voice note it can only name,
/// because silk is a format nothing here decodes and none of the alternatives
/// is worth carrying a codec for.
const ITEM_IMAGE: i64 = 2;
const ITEM_VOICE: i64 = 3;
const ITEM_FILE: i64 = 4;
const ITEM_VIDEO: i64 = 5;

/// The item type the platform itself puts on the `message_item` inside a
/// quote's `ref_msg`: 0 in every capture, inbound and outbound alike — the
/// reference stands for someone else's message, not for a fresh text item.
const QUOTED_REF: i64 = 0;
/// Sent by a bot rather than by a person.
const FROM_BOT: i64 = 2;
/// A complete message rather than one still being streamed in.
const FINISHED: i64 = 2;

/// Where the encrypted bytes of a picture or a document actually live. Distinct
/// from [`HOST`]: the API says who may upload and the CDN holds what they
/// uploaded, and the two are not the same machine.
const CDN: &str = "https://novac2c.cdn.weixin.qq.com/c2c";

/// `media_type` on an upload, which is numbered differently from the item type
/// the finished message carries — 1/2/3/4 here against 2/5/4/3 there. Two
/// numberings for four things is exactly the sort of detail that is silently
/// wrong forever, so neither is ever written as a literal at a call site.
const UPLOAD_IMAGE: i64 = 1;
const UPLOAD_FILE: i64 = 3;

/// The one encryption the CDN offers, named in the message so the phone on the
/// other end knows how to undo it.
const ENCRYPT_AES: i64 = 1;

/// AES works a block at a time, and this is the block.
const BLOCK: usize = 16;

/// The conversation has been quiet too long and WeChat has closed it. Only the
/// person can open it again, by saying something.
pub const ASLEEP: i64 = -14;

/// How long to hold a poll open. The protocol will wait thirty-five seconds,
/// but the rounds this is called from come by every few seconds and a request
/// still in flight is a round that cannot run — so it is cut short and asked
/// again. Nothing is lost by that: the cursor only moves on an answer.
const POLL_SECS: u64 = 4;

/// A login waiting to be scanned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Login {
    /// The server's handle for this attempt, which is what its progress is
    /// asked about. Not shown to anyone.
    pub handle: String,
    /// What the square encodes. WeChat hands back a link rather than a picture,
    /// which is the useful way round: a terminal can draw a link.
    pub link: String,
}

/// How far a scan has got.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Scan {
    /// Nobody has pointed a camera at it yet.
    Waiting,
    /// Scanned, and now waiting on the tap that allows it.
    Scanned,
    /// The code went stale. They last about five minutes.
    Expired,
    Connected(Box<Account>),
}

/// A bot, once somebody has agreed to it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Account {
    /// The bot's own id.
    pub bot_id: String,
    /// The bearer token every later call carries. A secret.
    pub token: String,
    /// The host to use from here on, which the login may move.
    pub base_url: String,
    /// Whoever scanned the code: the person this bot talks to.
    pub user_id: String,
}

/// One thing a person said to the bot.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Said {
    pub message_id: String,
    pub from: String,
    pub text: String,
    /// Epoch milliseconds, as WeChat recorded it.
    pub at: u64,
    /// The token that has to travel on anything said back. Every message in
    /// carries a fresh one, and the newest is the one that works.
    pub context_token: String,
    /// When this message was a quote-reply: the WeChat id of the message it
    /// quoted, as a string. That is the very id `send_text`'s reply body named
    /// when the quoted message went out, so it is what matches the quote back
    /// to its sender. `None` when they simply typed — the common case, and
    /// what every message looked like before this was read.
    pub quoted_id: Option<String>,
    /// What they attached, still on WeChat's CDN. Empty for the ordinary
    /// message, which is words and nothing else.
    pub media: Vec<Enclosure>,
}

/// One thing a person attached, as the message that carries it names it.
///
/// The bytes are not in here: they are on the CDN, encrypted, and [`fetch`] is
/// what turns one of these into a file. The handle and the key stay private on
/// purpose — the first is opaque and the second is a secret, and neither is
/// anything a caller has business holding or logging.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Enclosure {
    /// Whether the phone showed it in the conversation or as a row to tap.
    pub medium: Medium,
    /// What the person called it, when the message says. A picture arrives
    /// with no name at all and gets one from what its bytes turn out to be.
    pub name: String,
    /// The CDN's handle for the stored object.
    query_param: String,
    /// The AES key, in whichever of its spellings the message used.
    key: String,
}

/// What one round of asking for messages found.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Updates {
    pub said: Vec<Said>,
    /// Where to carry on from. Kept exactly as it came: it is the server's
    /// bookkeeping, not ours to read.
    pub cursor: String,
    /// The conversation is asleep and only the person can wake it.
    pub asleep: bool,
}

/// A nonce WeChat wants on every call, to keep an old request from being played
/// back at it: a random number, written out, and base64'd. Drawn from the clock
/// rather than from a random source, because nothing rests on it being
/// unguessable — only on it being different each time.
fn nonce() -> String {
    use std::sync::atomic::{AtomicU32, Ordering};
    static NEXT: AtomicU32 = AtomicU32::new(0);
    let spun = NEXT.fetch_add(0x9E37_79B9, Ordering::Relaxed);
    let clock = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since| since.subsec_nanos())
        .unwrap_or_default();
    base64(spun.wrapping_add(clock).to_string().as_bytes())
}

fn base64(input: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let block = chunk.iter().enumerate().fold(0u32, |block, (index, byte)| {
            block | (u32::from(*byte) << (16 - 8 * index))
        });
        for index in 0..=chunk.len() {
            out.push(ALPHABET[(block >> (18 - 6 * index)) as usize & 0x3F] as char);
        }
        for _ in chunk.len()..3 {
            out.push('=');
        }
    }
    out
}

/// An id for one outgoing message, so a retry cannot arrive twice.
fn client_id() -> String {
    format!("muxloom-{}", nonce().trim_end_matches('='))
}

fn headers<'a>(token: &'a str, nonce: &'a str) -> [(&'a str, &'a str); 3] {
    [
        ("AuthorizationType", "ilink_bot_token"),
        ("X-WECHAT-UIN", nonce),
        ("Authorization", token),
    ]
}

fn endpoint(base: &str, path: &str) -> String {
    format!("{}/ilink/bot/{path}", base.trim_end_matches('/'))
}

/// Ask for a code to scan.
pub fn begin(environment: &[(String, String)]) -> Result<Login> {
    let answer = http::get_json(
        &format!("{}?bot_type={BOT_TYPE}", endpoint(HOST, "get_bot_qrcode")),
        &[],
        environment,
    )?;
    let handle = text_at(&answer, "qrcode");
    let link = text_at(&answer, "qrcode_img_content");
    if handle.is_empty() || link.is_empty() {
        bail!("WeChat did not hand back a code to scan");
    }
    Ok(Login { handle, link })
}

/// Wait a few seconds for the scan to move on. Silence is `Waiting`: the caller
/// asks again, and the panel goes on drawing in between.
pub fn watch(handle: &str, environment: &[(String, String)]) -> Result<Scan> {
    let Some(answer) = http::poll_json(
        &format!(
            "{}?qrcode={}",
            endpoint(HOST, "get_qrcode_status"),
            urlencode(handle)
        ),
        &[("iLink-App-ClientVersion", "1")],
        None,
        POLL_SECS,
        environment,
    )?
    else {
        return Ok(Scan::Waiting);
    };
    match text_at(&answer, "status").as_str() {
        "confirmed" => {
            let account = Account {
                bot_id: text_at(&answer, "ilink_bot_id"),
                token: text_at(&answer, "bot_token"),
                base_url: Some(text_at(&answer, "baseurl"))
                    .filter(|url| !url.is_empty())
                    .unwrap_or_else(|| HOST.to_string()),
                user_id: text_at(&answer, "ilink_user_id"),
            };
            if account.token.is_empty() || account.user_id.is_empty() {
                bail!("WeChat allowed the connection but did not say who to talk to");
            }
            Ok(Scan::Connected(Box::new(account)))
        }
        "scaned" | "scanned" => Ok(Scan::Scanned),
        "expired" => Ok(Scan::Expired),
        _ => Ok(Scan::Waiting),
    }
}

/// WeChat's verdict on one send, read straight off the reply body.
///
/// WeChat answers HTTP 200 for a refused, a dropped, and a delivered send alike,
/// so the status line never says whether the message arrived. The code and
/// reason are the protocol's own complaint; `delivery_confirmed` is the one
/// thing this side cannot mint for itself — an id of WeChat's own in the body,
/// distinct from the `client_id` the request carried. An accepted-but-dropped
/// reply (a stale context token waved through as a success) comes back code 0
/// with no such field, which is exactly what "a success id that never arrives"
/// looks like.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Verdict {
    pub code: i64,
    pub reason: String,
    pub delivery_confirmed: bool,
    /// WeChat's own id for the message that went out, from the reply body
    /// (`message_id`), when the body carried one. This is the id a later quote
    /// of the message names, so it is the one a receipt has to keep: the
    /// `client_id` this side minted proves nothing to a reply.
    pub message_id: Option<String>,
}

/// Say one thing to the person this bot belongs to.
///
/// `context_token` is the one off the last thing they said. Without it WeChat
/// has no conversation to put the message in, so a bot nobody has greeted has
/// nothing to answer into — which the caller is expected to explain rather than
/// pass on as a protocol error.
///
/// Returns the id this side minted for the send together with WeChat's own
/// verdict on it. The id alone says nothing about delivery; the body does, and
/// the caller is the only one in a position to read it before it is forgotten.
///
/// `quoted_id` is the WeChat message id this send answers, when it answers
/// one: the same string a quote-reply arrives naming. With it the message
/// shows on the phone as quoting that one; without it, as a plain message.
pub fn send_text(
    account: &Account,
    context_token: &str,
    text: &str,
    quoted_id: Option<&str>,
    environment: &[(String, String)],
) -> Result<(String, Verdict)> {
    if context_token.trim().is_empty() {
        bail!("say anything to this bot in WeChat first — it can only answer a conversation");
    }
    let id = client_id();
    let nonce = nonce();
    let bearer = format!("Bearer {}", account.token.trim());
    let answer = http::post_json(
        &endpoint(&account.base_url, "sendmessage"),
        &headers(&bearer, &nonce),
        &send_body(&account.user_id, &id, context_token, text, quoted_id),
        environment,
    )?;
    capture_raw("send", &answer);
    let verdict = verdict_of(&answer, &id);
    // A reply that is accepted but never delivered is indistinguishable from a
    // real delivery unless the body is read: WeChat answers HTTP 200 for both
    // and puts the whole verdict in the code, the reason, and a delivery id it
    // only includes when the message actually goes out. Logging the platform's
    // own reply (code, reason, body keys — never values) keeps a stale context
    // token from leaving no trace at all.
    crate::debug::log(
        "ilink",
        format!(
            "sendmessage verdict: code={} reason={} delivery_confirmed={} body_keys={:?}",
            verdict.code,
            if verdict.reason.is_empty() {
                "none"
            } else {
                verdict.reason.as_str()
            },
            verdict.delivery_confirmed,
            answer
                .as_object()
                .map(|fields| fields.keys().cloned().collect::<Vec<_>>())
                .unwrap_or_default()
        ),
    );
    refused_send(&answer)?;
    Ok((id, verdict))
}

/// The outgoing body for one send, apart from the wire so the shape a quote
/// travels on can be checked without a network.
fn send_body(
    to: &str,
    id: &str,
    context_token: &str,
    text: &str,
    quoted_id: Option<&str>,
) -> Value {
    json!({
        "msg": {
            "to_user_id": to,
            "client_id": id,
            "message_type": FROM_BOT,
            "message_state": FINISHED,
            "context_token": context_token,
            "item_list": [text_item(text, quoted_id)],
        },
        "base_info": { "channel_version": env!("CARGO_PKG_VERSION") },
    })
}

/// One text item, quoting `quoted_id` when there is one.
///
/// The reference mirrors the shape a quote arrives in — `ref_msg` wrapping a
/// `message_item` whose `msg_id` is a string of the platform's numeric id —
/// because that is the only pairing of the two names ever captured off the
/// wire. Minimal past that, like every other item this bot sends: fields the
/// platform invented for its own bookkeeping (`create_time_ms` and the rest)
/// are not ours to guess at.
fn text_item(text: &str, quoted_id: Option<&str>) -> Value {
    let mut item = json!({ "type": ITEM_TEXT, "text_item": { "text": text } });
    if let Some(quoted) = quoted_id.filter(|id| !id.trim().is_empty()) {
        item["ref_msg"] = json!({
            "message_item": { "type": QUOTED_REF, "msg_id": quoted },
        });
    }
    item
}

/// The kind of thing being sent, which decides both of the protocol's two
/// numberings for it and how the finished item is shaped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Medium {
    /// Shown inline on the phone.
    Image,
    /// Arrives as a document with a name on it, to be tapped and saved.
    File,
}

impl Medium {
    /// What `getuploadurl` calls it.
    fn upload_type(self) -> i64 {
        match self {
            Medium::Image => UPLOAD_IMAGE,
            Medium::File => UPLOAD_FILE,
        }
    }

    /// What the item in the finished message calls it.
    fn item_type(self) -> i64 {
        match self {
            Medium::Image => ITEM_IMAGE,
            Medium::File => ITEM_FILE,
        }
    }
}

/// Send a picture or a document.
///
/// Three round trips, in an order that is not negotiable: ask the API where to
/// put it, put it there, then tell the conversation it exists. The bytes go up
/// encrypted under a key this side mints and then hands over in the message
/// itself — WeChat's CDN never sees the key, and the phone gets it by being in
/// the conversation. That is the whole of the scheme, and it means the key is
/// worth generating properly: [`nonce`] is drawn from the clock, which is fine
/// for a replay guard and would be a real weakness here, so this one comes from
/// the OS.
///
/// `caption` goes as a separate text message before the media, because the
/// protocol takes one item per send — a caption bundled into the same
/// `item_list` is dropped rather than refused, which is the worst way to lose
/// one.
pub fn send_media(
    account: &Account,
    context_token: &str,
    medium: Medium,
    file_name: &str,
    bytes: &[u8],
    environment: &[(String, String)],
) -> Result<(String, Verdict)> {
    if context_token.trim().is_empty() {
        bail!("say anything to this bot in WeChat first — it can only answer a conversation");
    }
    if bytes.is_empty() {
        bail!("{file_name} is empty, and WeChat will not carry an empty file");
    }
    let uploaded = upload(account, medium, bytes, environment)?;
    let id = client_id();
    let nonce = nonce();
    let bearer = format!("Bearer {}", account.token.trim());
    let answer = http::post_json(
        &endpoint(&account.base_url, "sendmessage"),
        &headers(&bearer, &nonce),
        &media_body(
            &account.user_id,
            &id,
            context_token,
            medium,
            file_name,
            &uploaded,
        ),
        environment,
    )?;
    capture_raw("send-media", &answer);
    let verdict = verdict_of(&answer, &id);
    crate::debug::log(
        "ilink",
        format!(
            "sendmedia verdict: kind={medium:?} bytes={} code={} reason={} delivery_confirmed={}",
            bytes.len(),
            verdict.code,
            if verdict.reason.is_empty() {
                "none"
            } else {
                verdict.reason.as_str()
            },
            verdict.delivery_confirmed,
        ),
    );
    refused_send(&answer)?;
    Ok((id, verdict))
}

/// What an upload leaves behind: everything the message then has to name.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Uploaded {
    /// The CDN's handle for the stored object, which is what a download asks
    /// for.
    query_param: String,
    /// The key the bytes were encrypted under, base64 of the raw sixteen. The
    /// phone cannot read the file without it.
    aes_key: String,
    /// The file as the sender sees it.
    raw_size: usize,
    /// The file as the CDN stores it, which is longer: PKCS#7 always adds at
    /// least one byte. The image item reports this one, and getting it wrong
    /// truncates the picture.
    stored_size: usize,
}

/// Put the bytes on the CDN and come back with the handle for them.
fn upload(
    account: &Account,
    medium: Medium,
    bytes: &[u8],
    environment: &[(String, String)],
) -> Result<Uploaded> {
    let key = random_key()?;
    let file_key = hex(&random_key()?);
    let nonce = nonce();
    let bearer = format!("Bearer {}", account.token.trim());
    let stored_size = padded_len(bytes.len());
    let permission = http::post_json(
        &endpoint(&account.base_url, "getuploadurl"),
        &headers(&bearer, &nonce),
        &json!({
            "filekey": file_key,
            "media_type": medium.upload_type(),
            "to_user_id": account.user_id,
            "rawsize": bytes.len(),
            "rawfilemd5": md5_hex(bytes),
            "filesize": stored_size,
            "no_need_thumb": true,
            "aeskey": hex(&key),
            "base_info": { "channel_version": env!("CARGO_PKG_VERSION") },
        }),
        environment,
    )?;
    capture_raw("upload-url", &permission);
    let upload_param = text_at(&permission, "upload_param");
    if upload_param.is_empty() {
        bail!(
            "WeChat would not say where to put the file: {}",
            one_line(&permission)
        );
    }
    // Everything past here is the CDN rather than the API, so a refusal comes
    // back as a status rather than as a code in a body.
    let answer = http::post_bytes(
        &format!(
            "{CDN}/upload?encrypted_query_param={}&filekey={}",
            urlencode(&upload_param),
            urlencode(&file_key)
        ),
        &[],
        "application/octet-stream",
        &encrypt(bytes, &key),
        environment,
    )?;
    // The stored object's handle comes back in a header and nowhere else — an
    // empty body with a 200 is the success case, so not finding the header is
    // the failure that has to be caught here rather than three lines later as a
    // message referring to nothing.
    let query_param = answer
        .header("x-encrypted-param")
        .unwrap_or_default()
        .to_string();
    if query_param.is_empty() {
        bail!("WeChat's CDN took the file but did not say where it put it");
    }
    Ok(Uploaded {
        query_param,
        aes_key: base64(&key),
        raw_size: bytes.len(),
        stored_size,
    })
}

/// The outgoing body for one media send, apart from the wire so its shape can
/// be checked without a network.
fn media_body(
    to: &str,
    id: &str,
    context_token: &str,
    medium: Medium,
    file_name: &str,
    uploaded: &Uploaded,
) -> Value {
    let media = json!({
        "encrypt_query_param": uploaded.query_param,
        "aes_key": uploaded.aes_key,
        "encrypt_type": ENCRYPT_AES,
    });
    // The two items disagree about which size they want and what they call it.
    // An image reports the stored (encrypted, padded) length; a file reports
    // the real one, as a string, because that is the number shown under the
    // document's name on the phone.
    let body = match medium {
        Medium::Image => json!({ "media": media, "mid_size": uploaded.stored_size }),
        Medium::File => json!({
            "media": media,
            "file_name": file_name,
            "len": uploaded.raw_size.to_string(),
        }),
    };
    let field = match medium {
        Medium::Image => "image_item",
        Medium::File => "file_item",
    };
    json!({
        "msg": {
            "to_user_id": to,
            "client_id": id,
            "message_type": FROM_BOT,
            "message_state": FINISHED,
            "context_token": context_token,
            "item_list": [{ "type": medium.item_type(), field: body }],
        },
        "base_info": { "channel_version": env!("CARGO_PKG_VERSION") },
    })
}

/// Sixteen bytes from the operating system's own source.
///
/// An error here is not survivable by falling back to something weaker: the
/// caller is about to encrypt somebody's file, and a key from a worse source
/// would be a silent downgrade of the only protection the bytes get.
fn random_key() -> Result<[u8; BLOCK]> {
    let mut key = [0u8; BLOCK];
    getrandom::getrandom(&mut key)
        .map_err(|error| anyhow!("this machine would not give out a random key: {error}"))?;
    Ok(key)
}

/// AES-128-ECB with PKCS#7 padding, which is what the CDN reads.
///
/// ECB is the platform's choice, not one open to us: a message naming any other
/// mode is refused. It leaks equal blocks, which for a picture is a real
/// weakness — but the alternative on offer is not a better mode, it is sending
/// nothing.
fn encrypt(bytes: &[u8], key: &[u8; BLOCK]) -> Vec<u8> {
    use aes::Aes128;
    use aes::cipher::{BlockEncrypt, KeyInit, generic_array::GenericArray};
    let cipher = Aes128::new(GenericArray::from_slice(key));
    let mut out = Vec::with_capacity(padded_len(bytes.len()));
    out.extend_from_slice(bytes);
    // PKCS#7: pad to the next block with the pad length repeated, and add a
    // whole block when the input already fits — so the far side can always
    // trust the last byte and there is no length to send separately.
    let pad = BLOCK - (bytes.len() % BLOCK);
    out.extend(std::iter::repeat_n(pad as u8, pad));
    for block in out.chunks_mut(BLOCK) {
        cipher.encrypt_block(GenericArray::from_mut_slice(block));
    }
    out
}

/// How long `bytes` becomes once padded — always strictly longer, never equal.
fn padded_len(len: usize) -> usize {
    len + BLOCK - (len % BLOCK)
}

/// The file's MD5, lowercase hex, which the upload declares up front so the CDN
/// can tell a truncated arrival from a whole one. Not a security claim, and not
/// used as one: the confidentiality here is the AES key.
fn md5_hex(bytes: &[u8]) -> String {
    use md5::{Digest, Md5};
    hex(&Md5::digest(bytes))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().fold(String::new(), |mut out, byte| {
        use std::fmt::Write;
        let _ = write!(out, "{byte:02x}");
        out
    })
}

/// A refusal body squeezed onto one line, for an error message. Trimmed,
/// because these can be long and the useful part is at the front.
fn one_line(answer: &Value) -> String {
    let text = answer.to_string();
    match text.char_indices().nth(200) {
        Some((cut, _)) => format!("{}…", &text[..cut]),
        None => text,
    }
}

/// Read the verdict off one reply body: the code, the human reason, whether
/// the body carries a delivery id of WeChat's own — a server-side id distinct
/// from the `client_id` this side sent, which is the only evidence the message
/// went out rather than merely being accepted (an accepted-but-dropped reply
/// (a stale context token) comes back with none) — and that id itself, which
/// is what a later quote of the message will name.
fn verdict_of(answer: &Value, client_id: &str) -> Verdict {
    let code = code_of(answer);
    let reason = answer
        .get("errmsg")
        .and_then(Value::as_str)
        .filter(|reason| !reason.is_empty())
        .map(str::to_string)
        .unwrap_or_default();
    let delivery_confirmed = answer.as_object().is_some_and(|fields| {
        fields
            .iter()
            .filter(|(key, _)| key.to_ascii_lowercase().contains("id"))
            .filter_map(|(_, value)| match value {
                Value::String(text) if !text.trim().is_empty() => Some(text.clone()),
                Value::Number(number) => Some(number.to_string()),
                _ => None,
            })
            .any(|value| value != client_id)
    });
    let message_id = Some(number_or_text(answer, "message_id")).filter(|id| !id.is_empty());
    Verdict {
        code,
        reason,
        delivery_confirmed,
        message_id,
    }
}

/// Diagnostic escape hatch for pinning down what the platform actually sends:
/// when `MUXLOOM_ILINK_CAPTURE` names a directory, every raw `getupdates` and
/// `sendmessage` reply body is written there as one pretty-printed JSON file
/// per call. Off unless the variable is set; the bodies carry context tokens,
/// so the directory must stay private (files land 0600).
fn capture_raw(kind: &str, answer: &Value) {
    let Ok(dir) = std::env::var("MUXLOOM_ILINK_CAPTURE") else {
        return;
    };
    if dir.is_empty() {
        return;
    }
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since| since.as_millis())
        .unwrap_or_default();
    write_raw_capture(Path::new(&dir), kind, stamp, answer);
}

/// One captured body, one file: `ilink-<kind>-<unix-ms>.json`.
fn write_raw_capture(dir: &Path, kind: &str, stamp: u128, answer: &Value) {
    let Ok(bytes) = serde_json::to_vec_pretty(answer) else {
        return;
    };
    if std::fs::create_dir_all(dir).is_err() {
        return;
    }
    let path = dir.join(format!("ilink-{kind}-{stamp}.json"));
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .mode(0o600)
            .open(&path)
        else {
            return;
        };
        use std::io::Write;
        let _ = file.write_all(&bytes);
    }
    #[cfg(not(unix))]
    {
        let _ = std::fs::write(&path, &bytes);
    }
}

/// Ask for whatever has been said since `cursor`.
pub fn updates(
    account: &Account,
    cursor: &str,
    environment: &[(String, String)],
) -> Result<Updates> {
    let nonce = nonce();
    let bearer = format!("Bearer {}", account.token.trim());
    let Some(answer) = http::poll_json(
        &endpoint(&account.base_url, "getupdates"),
        &headers(&bearer, &nonce),
        Some(&json!({
            "get_updates_buf": cursor,
            "base_info": { "channel_version": env!("CARGO_PKG_VERSION") },
        })),
        POLL_SECS,
        environment,
    )?
    else {
        // Held open and nothing said. Keep the cursor and come back.
        return Ok(Updates {
            cursor: cursor.to_string(),
            ..Updates::default()
        });
    };
    capture_raw("updates", &answer);
    if code_of(&answer) == ASLEEP {
        return Ok(Updates {
            cursor: cursor.to_string(),
            asleep: true,
            ..Updates::default()
        });
    }
    complain(&answer)?;
    let msgs = answer
        .get("msgs")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default();
    Ok(Updates {
        said: parse_said(msgs),
        cursor: Some(text_at(&answer, "get_updates_buf"))
            .filter(|moved| !moved.is_empty())
            .unwrap_or_else(|| cursor.to_string()),
        ..Updates::default()
    })
}

/// Turn one batch of inbound messages into [`Said`], oldest first.
///
/// Only what a person typed survives: the bot's own words come back through the
/// same window, and reading those would be a chat talking to itself.
///
/// The order is the point. WeChat does not promise the order of `msgs`, and the
/// token that answers a conversation is the one off the newest message. Every
/// consumer downstream — `channel::absorb` and the panel's "last one wins" —
/// assumes oldest-first, so a reordered reply would silently pin a stale
/// context token and mute the bot. Sort here, once, rather than trusting the
/// wire.
fn parse_said(msgs: &[Value]) -> Vec<Said> {
    let mut said = msgs
        .iter()
        .filter(|message| message.get("message_type").and_then(Value::as_i64) == Some(1))
        .filter_map(|message| {
            // Words, an attachment, or both. Neither is not a message: a
            // reaction or a card being edited comes through this same window,
            // and waking an agent with nothing in hand is worse than silence.
            let media = enclosures(message);
            let text = spoken(message).unwrap_or_default();
            if text.trim().is_empty() && media.is_empty() {
                return None;
            }
            Some(Said {
                message_id: number_or_text(message, "message_id"),
                from: text_at(message, "from_user_id"),
                text,
                at: message
                    .get("create_time_ms")
                    .and_then(Value::as_u64)
                    .unwrap_or_default(),
                context_token: text_at(message, "context_token"),
                quoted_id: quoted(message),
                media,
            })
        })
        .collect::<Vec<_>>();
    said.sort_by_key(|said| said.at);
    said
}

/// The words out of one message, and only those. What is not words is either
/// fetched — see [`enclosures`] — or, in the one case nothing here can open,
/// named by [`missing`].
fn spoken(message: &Value) -> Option<String> {
    let text = message
        .get("item_list")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default()
        .iter()
        .filter_map(|item| {
            // A voice note arrives transcribed, which is as good as typed.
            let said = item
                .pointer("/text_item/text")
                .or_else(|| item.pointer("/voice_item/text"))
                .and_then(Value::as_str)
                .map(str::to_string)
                .filter(|said| !said.trim().is_empty());
            // Anything else is either fetched elsewhere or, for the one kind
            // that cannot be, named. Saying so is worth doing: the alternative
            // is that something reaches the machine as silence, and whoever
            // sent it watches nobody answer.
            said.or_else(|| Some(missing(item.get("type").and_then(Value::as_i64)?)?.to_string()))
        })
        .collect::<Vec<_>>()
        .join("\n");
    Some(text).filter(|text| !text.trim().is_empty())
}

/// What to say instead of an item nothing here can open.
///
/// A voice note that came without a transcription is the whole of that list.
/// It arrives as silk, which no codec on this machine decodes, so the honest
/// thing is to say it arrived: the agent reading this has to answer the person
/// without ever hearing it. Everything else non-textual is fetched.
fn missing(kind: i64) -> Option<&'static str> {
    match kind {
        ITEM_VOICE => Some("(they sent a voice message that arrived without a transcription.)"),
        _ => None,
    }
}

/// Everything one message carries that can actually be had.
///
/// Deliberately not everything that is not words. A voice note is left out: it
/// is silk, and a file nobody on this machine can open is worse than a line
/// saying one arrived, because the line at least tells an agent to ask.
fn enclosures(message: &Value) -> Vec<Enclosure> {
    message
        .get("item_list")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default()
        .iter()
        .filter_map(|item| {
            let (body, medium, name) = match item.get("type").and_then(Value::as_i64)? {
                ITEM_IMAGE => (item.get("image_item")?, Medium::Image, String::new()),
                ITEM_FILE => {
                    let body = item.get("file_item")?;
                    let name = text_at(body, "file_name");
                    (body, Medium::File, name)
                }
                // A video is a document as far as this end is concerned. It
                // arrives unnamed, and mp4 is what the phone sends.
                ITEM_VIDEO => (
                    item.get("video_item")?,
                    Medium::File,
                    "video.mp4".to_string(),
                ),
                _ => return None,
            };
            let media = body.get("media")?;
            let query_param = text_at(media, "encrypt_query_param");
            let key = match medium {
                // A picture names its key twice — `aeskey` beside the item, in
                // hex, and `media.aes_key` in base64 — and the two have been
                // seen to disagree. WeChat's own client reads the first, so
                // this does too, and falls back rather than failing.
                Medium::Image => Some(text_at(body, "aeskey")).filter(|key| !key.is_empty()),
                Medium::File => None,
            }
            .unwrap_or_else(|| text_at(media, "aes_key"));
            match query_param.is_empty() {
                true => None,
                false => Some(Enclosure {
                    medium,
                    name,
                    query_param,
                    key,
                }),
            }
        })
        .collect()
}

/// Fetch what somebody attached, and hand back the bytes they attached.
///
/// One round trip and then the undoing of what [`send_media`] does going the
/// other way. The CDN holds ciphertext and has never held the key: that
/// travels in the message, which is why this is reachable at all without
/// asking the API for permission first.
pub fn fetch(enclosure: &Enclosure, environment: &[(String, String)]) -> Result<Vec<u8>> {
    let bytes = http::get_bytes(
        &format!(
            "{CDN}/download?encrypted_query_param={}",
            urlencode(&enclosure.query_param)
        ),
        &[],
        environment,
    )?;
    if enclosure.key.trim().is_empty() {
        // Nothing named a key, so there is nothing to undo: an object stored
        // in the clear comes back usable as it stands.
        return Ok(bytes);
    }
    decrypt(&bytes, &parse_key(&enclosure.key)?)
}

/// The sixteen bytes of an AES key, out of however the message spelled it.
///
/// Three spellings are in the wild for the same key and nothing in the message
/// says which one it used: thirty-two hex characters as they stand, base64 of
/// the sixteen raw bytes, and base64 of those same thirty-two hex characters.
/// Only the length tells them apart, which is why this is a function rather
/// than a call to a decoder.
fn parse_key(spelling: &str) -> Result<[u8; BLOCK]> {
    let spelling = spelling.trim();
    let bytes = match unhex(spelling) {
        Some(raw) if raw.len() == BLOCK => raw,
        _ => {
            let decoded = unbase64(spelling)
                .ok_or_else(|| anyhow!("this message spells its key in neither hex nor base64"))?;
            match unhex(&String::from_utf8_lossy(&decoded)) {
                Some(raw) if raw.len() == BLOCK => raw,
                _ => decoded,
            }
        }
    };
    <[u8; BLOCK]>::try_from(bytes.as_slice()).map_err(|_| {
        anyhow!(
            "this message's key is {} bytes rather than {BLOCK}",
            bytes.len()
        )
    })
}

/// Bytes out of hex, or nothing at all. Half an answer is not useful here: a
/// key decoded part of the way is a file silently ruined.
fn unhex(text: &str) -> Option<Vec<u8>> {
    if text.is_empty() || text.len() % 2 != 0 || !text.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return None;
    }
    (0..text.len())
        .step_by(2)
        .map(|at| u8::from_str_radix(&text[at..at + 2], 16).ok())
        .collect()
}

/// The inverse of [`base64`]. Padding and whitespace are ignored and the
/// url-safe alphabet is accepted, because the far side is several
/// implementations and they do not agree; anything else outside the alphabet
/// makes the whole thing a refusal rather than a shorter answer.
fn unbase64(text: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(text.len() / 4 * 3);
    let mut block = 0u32;
    let mut held = 0u32;
    for byte in text.bytes() {
        if byte == b'=' || byte.is_ascii_whitespace() {
            continue;
        }
        let index = match byte {
            b'A'..=b'Z' => u32::from(byte - b'A'),
            b'a'..=b'z' => u32::from(byte - b'a') + 26,
            b'0'..=b'9' => u32::from(byte - b'0') + 52,
            b'+' | b'-' => 62,
            b'/' | b'_' => 63,
            _ => return None,
        };
        block = (block << 6) | index;
        held += 6;
        if held >= 8 {
            held -= 8;
            out.push((block >> held) as u8);
        }
    }
    Some(out)
}

/// The undoing of [`encrypt`]: AES-128-ECB, with the padding checked rather
/// than merely trimmed off.
///
/// The check is the point. A wrong key decrypts every bit as happily as a
/// right one and hands back noise, and noise ending in a byte between 1 and 16
/// would be written to disk as somebody's photograph. Padding that does not
/// hold together is the only signal that the key was wrong, and this is the
/// last place to catch it before the file is on the machine and broken.
fn decrypt(bytes: &[u8], key: &[u8; BLOCK]) -> Result<Vec<u8>> {
    use aes::Aes128;
    use aes::cipher::{BlockDecrypt, KeyInit, generic_array::GenericArray};
    if bytes.is_empty() || bytes.len() % BLOCK != 0 {
        bail!(
            "WeChat's CDN answered with {} bytes, which is not whole AES blocks",
            bytes.len()
        );
    }
    let cipher = Aes128::new(GenericArray::from_slice(key));
    let mut out = bytes.to_vec();
    for block in out.chunks_mut(BLOCK) {
        cipher.decrypt_block(GenericArray::from_mut_slice(block));
    }
    let pad = usize::from(out.last().copied().unwrap_or_default());
    let padded = (1..=BLOCK).contains(&pad)
        && out.len() >= pad
        && out[out.len() - pad..]
            .iter()
            .all(|byte| usize::from(*byte) == pad);
    if !padded {
        bail!(
            "this file did not decrypt — the key on the message does not fit the bytes the CDN held"
        );
    }
    out.truncate(out.len() - pad);
    Ok(out)
}

/// The message one of these quotes, when the person replied by quoting one.
///
/// A quote-reply is an ordinary message whose item carries a `ref_msg` with a
/// `message_item` naming the quoted message by `msg_id` — the platform id the
/// original send's reply body handed back, as a string. The first item that
/// names something wins; a person quotes one message per message, and an item
/// list with more than one reference is not a thing the captures show.
fn quoted(message: &Value) -> Option<String> {
    message
        .get("item_list")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default()
        .iter()
        .find_map(|item| {
            Some(number_or_text(
                item.pointer("/ref_msg/message_item")?,
                "msg_id",
            ))
            .filter(|id| !id.is_empty())
        })
}

/// WeChat answers HTTP 200 and puts its refusal in the body, so the status line
/// never says whether anything happened.
fn complain(answer: &Value) -> Result<()> {
    match code_of(answer) {
        0 => Ok(()),
        ASLEEP => bail!(
            "WeChat has closed this conversation — say anything to the bot there and it opens again"
        ),
        code => bail!(
            "WeChat refused it: {} (ret {code})",
            answer
                .get("errmsg")
                .and_then(Value::as_str)
                .filter(|reason| !reason.is_empty())
                .unwrap_or("no reason given"),
        ),
    }
}

/// The same refusal, read as a send.
///
/// [`complain`] answers for every call this protocol makes, so it can only
/// repeat what the platform said. Two things are known here that are not known
/// there, and both are what somebody staring at `ret -2` actually wants.
///
/// The first is that WeChat is up. A refusal is read off a 2xx body, so having
/// one in hand means the request arrived and was answered; an outage never gets
/// this far — it ends in the transport, with a sentence about the host. So
/// whatever is wrong is wrong about this bot rather than about WeChat, and a
/// caller reading the platform's own terse complaint as "the service is down"
/// is reading it backwards.
///
/// The second is that a send is the one call standing on a context token, and a
/// token gone stale looks from here exactly like the platform being unhappy for
/// some other reason: the same 200, the same shape of body. The repair is the
/// same either way, and it is the only repair this side has, so the refusal
/// carries it — rather than leaving the caller to send again, and again, into a
/// refusal that no amount of sending clears.
fn refused_send(answer: &Value) -> Result<()> {
    match code_of(answer) {
        // One is not a refusal at all, and the other already says the only
        // thing worth saying about itself.
        0 | ASLEEP => complain(answer),
        _ => complain(answer).map_err(|refusal| {
            anyhow!(
                "{refusal}. WeChat answered, so this is not an outage — it is this bot the \
                 platform is unhappy with. Suspect the conversation token first: every message \
                 out carries the token off the last message in, only an inbound message renews \
                 one, and a stale one is the same dead end as a bot nobody has greeted. The \
                 person saying anything at all to it clears that; sending again does not."
            )
        }),
    }
}

/// The complaint code, under either of the two names the protocol uses for it.
fn code_of(answer: &Value) -> i64 {
    answer
        .get("errcode")
        .and_then(Value::as_i64)
        .filter(|code| *code != 0)
        .or_else(|| answer.get("ret").and_then(Value::as_i64))
        .unwrap_or_default()
}

fn text_at(value: &Value, key: &str) -> String {
    value
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

/// A field that is a number in one build of the protocol and a string in
/// another. Message ids are the usual one.
fn number_or_text(value: &Value, key: &str) -> String {
    match value.get(key) {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Number(number)) => number.to_string(),
        _ => String::new(),
    }
}

/// Percent-encode everything that is not plainly safe. The handle is opaque and
/// has been seen to carry `+` and `/`, either of which changes meaning if it
/// goes into a query string as itself.
fn urlencode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_nonce_is_different_every_time_it_is_asked_for() {
        let seen: std::collections::HashSet<String> = (0..64).map(|_| nonce()).collect();
        assert_eq!(seen.len(), 64, "a replayable nonce is not a nonce");
    }

    #[test]
    fn base64_matches_what_the_header_is_supposed_to_carry() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
        assert_eq!(base64(b"4283923842"), "NDI4MzkyMzg0Mg==");
    }

    #[test]
    fn an_opaque_handle_survives_being_put_in_a_query_string() {
        assert_eq!(urlencode("a+b/c=d"), "a%2Bb%2Fc%3Dd");
        assert_eq!(urlencode("plain-handle_1.2~3"), "plain-handle_1.2~3");
    }

    #[test]
    fn a_refusal_is_read_off_the_body_because_the_status_line_says_nothing() {
        assert!(complain(&json!({ "ret": 0 })).is_ok());
        assert!(complain(&json!({})).is_ok());
        let error = complain(&json!({ "ret": 42, "errmsg": "no such user" })).unwrap_err();
        assert!(format!("{error:#}").contains("no such user"), "{error:#}");
        // The one refusal that is not a fault: only the person can undo it, so
        // it says how rather than what went wrong.
        let asleep = complain(&json!({ "errcode": ASLEEP })).unwrap_err();
        assert!(
            format!("{asleep:#}").contains("say anything to the bot"),
            "{asleep:#}"
        );
    }

    /// A refused send is the one refusal a caller can act on, so it has to say
    /// which way to act: not at WeChat, and not by trying again.
    #[test]
    fn a_refused_send_rules_out_an_outage_and_names_the_repair() {
        let refused = refused_send(&json!({ "ret": -2, "errmsg": "prepare failed" })).unwrap_err();
        let refused = format!("{refused:#}");
        // The platform's own words survive; what is added is what this side
        // knows and the body never says.
        assert!(refused.contains("prepare failed (ret -2)"), "{refused}");
        assert!(refused.contains("not an outage"), "{refused}");
        assert!(refused.contains("conversation token"), "{refused}");
        // A send that went through is not a refusal, and a sleeping
        // conversation already reads correctly on its own.
        assert!(refused_send(&json!({ "ret": 0 })).is_ok());
        let asleep = refused_send(&json!({ "errcode": ASLEEP })).unwrap_err();
        let asleep = format!("{asleep:#}");
        assert!(asleep.contains("say anything to the bot"), "{asleep}");
        assert!(!asleep.contains("not an outage"), "{asleep}");
    }

    #[test]
    fn only_what_a_person_typed_is_read_out_of_a_message() {
        assert_eq!(
            spoken(&json!({ "item_list": [{ "type": 1, "text_item": { "text": "在" } }] })),
            Some("在".into())
        );
        // A voice note comes transcribed, which is as good as typed.
        assert_eq!(
            spoken(&json!({ "item_list": [{ "type": 3, "voice_item": { "text": "开会去了" } }] })),
            Some("开会去了".into())
        );
        // A picture is not words. It is not lost either — `enclosures` has it
        // — and saying something here as well would describe it twice.
        assert_eq!(
            spoken(&json!({ "item_list": [{ "type": 2, "image_item": {} }] })),
            None
        );
        // A voice note with no transcription is the one thing left that
        // nothing here can open, so it is still named rather than dropped.
        let voice = spoken(&json!({ "item_list": [{ "type": 3, "voice_item": {} }] }))
            .expect("an untranscribed voice note must not arrive as silence");
        assert!(voice.contains("voice message"), "{voice}");
        // An item type nobody has seen stays silent rather than inventing a
        // description of something that might be structural.
        assert_eq!(spoken(&json!({ "item_list": [{ "type": 97 }] })), None);
        assert_eq!(spoken(&json!({ "item_list": [] })), None);
        assert_eq!(spoken(&json!({})), None);
    }

    #[test]
    fn inbound_is_ordered_oldest_first_no_matter_the_wire_order() {
        // The reply token is the one off the newest message, and the consumers
        // of `said` assume oldest-first, so a reordered wire must not pin a
        // stale context token.
        let msgs = vec![
            json!({ "message_type": 1, "create_time_ms": 3000, "context_token": "newest",
                     "item_list": [{ "type": 1, "text_item": { "text": "c" } }] }),
            json!({ "message_type": 1, "create_time_ms": 1000, "context_token": "oldest",
                     "item_list": [{ "type": 1, "text_item": { "text": "a" } }] }),
            json!({ "message_type": 1, "create_time_ms": 2000, "context_token": "middle",
                     "item_list": [{ "type": 1, "text_item": { "text": "b" } }] }),
        ];
        let said = parse_said(&msgs);
        assert_eq!(
            said.iter()
                .map(|s| s.context_token.as_str())
                .collect::<Vec<_>>(),
            vec!["oldest", "middle", "newest"],
            "the newest message — and its token — must come last"
        );
        // Bot-sent messages are left out: a chat must not read its own words.
        let with_bot = vec![
            msgs[1].clone(),
            json!({ "message_type": 2, "create_time_ms": 1500,
                     "item_list": [{ "type": 1, "text_item": { "text": "bot" } }] }),
        ];
        assert_eq!(parse_said(&with_bot).len(), 1);
    }

    #[test]
    fn a_message_id_reads_the_same_whichever_shape_it_arrives_in() {
        assert_eq!(
            number_or_text(&json!({ "message_id": 8123 }), "message_id"),
            "8123"
        );
        assert_eq!(
            number_or_text(&json!({ "message_id": "8123" }), "message_id"),
            "8123"
        );
        assert_eq!(number_or_text(&json!({}), "message_id"), "");
    }

    #[test]
    fn nothing_is_sent_into_a_conversation_that_was_never_opened() {
        let error = send_text(&Account::default(), "  ", "hello", None, &[]).unwrap_err();
        assert!(
            format!("{error:#}").contains("say anything to this bot"),
            "{error:#}"
        );
    }

    #[test]
    fn the_verdict_tells_an_acceptance_apart_from_a_delivery() {
        let client_id = "muxloom-minted";
        // A delivery id of WeChat's own is the only evidence of delivery.
        assert_eq!(
            verdict_of(
                &json!({ "errcode": 0, "errmsg": "success", "msg_id": "wx-42" }),
                client_id
            ),
            Verdict {
                code: 0,
                reason: "success".into(),
                delivery_confirmed: true,
                message_id: None,
            }
        );
        // Code 0 with only the complaint keys: accepted, delivery unconfirmed.
        assert_eq!(
            verdict_of(&json!({ "errcode": 0, "errmsg": "success" }), client_id),
            Verdict {
                code: 0,
                reason: "success".into(),
                delivery_confirmed: false,
                message_id: None,
            }
        );
        // An echo of the id this side minted is not a delivery.
        assert_eq!(
            verdict_of(&json!({ "errcode": 0, "client_id": client_id }), client_id),
            Verdict {
                code: 0,
                reason: String::new(),
                delivery_confirmed: false,
                message_id: None,
            }
        );
        // A numeric delivery id counts, and a refusal carries its code.
        assert!(
            verdict_of(&json!({ "errcode": 0, "item_id": 8123 }), client_id).delivery_confirmed
        );
        let asleep = verdict_of(&json!({ "errcode": ASLEEP }), client_id);
        assert_eq!(asleep.code, ASLEEP);
        assert!(!asleep.delivery_confirmed);
        assert_eq!(asleep.message_id, None);
    }

    #[test]
    fn the_reply_body_hands_back_the_id_a_later_quote_will_name() {
        // Exactly the captured shape: a bare body whose `message_id` is a
        // number. A later quote-reply names this very value as a string, so
        // it has to come off the verdict in string form, whole.
        let verdict = verdict_of(
            &json!({ "message_id": 7498971037873973384u64 }),
            "muxloom-x",
        );
        assert_eq!(verdict.message_id.as_deref(), Some("7498971037873973384"));
        assert!(verdict.delivery_confirmed);
        // A body that names nothing — the accepted-but-dropped reply — hands
        // back nothing; no id is invented.
        assert_eq!(
            verdict_of(&json!({ "errcode": 0, "errmsg": "success" }), "muxloom-x").message_id,
            None
        );
    }

    #[test]
    fn a_quoted_reply_arrives_naming_the_message_it_quotes() {
        // The captured inbound shape: an ordinary message whose text item
        // carries a ref_msg whose message_item names the sent message by its
        // platform id, as a string.
        let msgs = vec![json!({
            "message_type": 1, "message_id": 7498971246643971720u64, "create_time_ms": 1000,
            "context_token": "ctx",
            "item_list": [{
                "type": 1,
                "msg_id": "v1:7593072987848423685",
                "ref_msg": { "message_item": { "type": 0, "msg_id": "7498971037873973384" } },
                "text_item": { "text": "你好你好" },
            }],
        })];
        let said = parse_said(&msgs);
        assert_eq!(said.len(), 1);
        assert_eq!(said[0].quoted_id.as_deref(), Some("7498971037873973384"));
        // And the very id the send handed back is the id the quote names.
        let sent = verdict_of(
            &json!({ "message_id": 7498971037873973384u64 }),
            "muxloom-x",
        );
        assert_eq!(sent.message_id, said[0].quoted_id);
    }

    #[test]
    fn a_plain_message_quotes_nothing() {
        let msgs = vec![json!({
            "message_type": 1, "message_id": 1u64, "create_time_ms": 1000,
            "context_token": "ctx",
            "item_list": [{ "type": 1, "text_item": { "text": "hi" } }],
        })];
        let said = parse_said(&msgs);
        assert_eq!(said[0].quoted_id, None);
        // An item that references nothing readable is still no quote.
        let empty_ref = vec![json!({
            "message_type": 1, "message_id": 2u64, "create_time_ms": 1000,
            "item_list": [{ "type": 1, "ref_msg": {}, "text_item": { "text": "hi" } }],
        })];
        assert_eq!(parse_said(&empty_ref)[0].quoted_id, None);
    }

    #[test]
    fn a_quoted_send_carries_the_reference_the_wire_shows() {
        // Without one, the body is exactly what it always was: a plain text
        // item and no reference anywhere.
        let plain = send_body("u@im.wechat", "muxloom-x", "ctx", "hi", None);
        assert_eq!(
            plain["msg"]["item_list"],
            json!([{ "type": ITEM_TEXT, "text_item": { "text": "hi" } }])
        );
        // With one, the reference mirrors the inbound shape: `ref_msg` around
        // a `message_item`, and the id travels as a STRING.
        let quoting = send_body(
            "u@im.wechat",
            "muxloom-x",
            "ctx",
            "hi",
            Some("7498971037873973384"),
        );
        assert_eq!(
            quoting["msg"]["item_list"][0]["ref_msg"],
            json!({ "message_item": { "type": QUOTED_REF, "msg_id": "7498971037873973384" } })
        );
        assert!(quoting["msg"]["item_list"][0]["ref_msg"]["message_item"]["msg_id"].is_string());
        // An empty id is no quote: it must not ride along as a broken one.
        let blank = send_body("u@im.wechat", "muxloom-x", "ctx", "hi", Some("  "));
        assert!(blank["msg"]["item_list"][0].get("ref_msg").is_none());
    }

    /// The protocol numbers the same four things twice, differently, and the
    /// two numberings are never both visible at one call site. Pinning them
    /// together is the only place the disagreement can be seen at all.
    #[test]
    fn a_picture_and_a_document_are_numbered_the_way_each_call_wants_them() {
        assert_eq!(Medium::Image.upload_type(), 1);
        assert_eq!(Medium::Image.item_type(), 2);
        assert_eq!(Medium::File.upload_type(), 3);
        assert_eq!(Medium::File.item_type(), 4);
    }

    /// PKCS#7 never leaves the length alone, which is why an image reports its
    /// stored size rather than its real one. A padded_len that returned the
    /// input for a whole number of blocks would truncate the last block of
    /// every file whose size happened to divide by sixteen.
    #[test]
    fn padding_always_grows_the_file_so_the_last_byte_can_be_trusted() {
        assert_eq!(padded_len(0), 16);
        assert_eq!(padded_len(1), 16);
        assert_eq!(padded_len(15), 16);
        assert_eq!(padded_len(16), 32, "a whole block still gains a block");
        assert_eq!(padded_len(17), 32);
    }

    /// Against FIPS-197's own AES-128 vector, so a wrong key schedule or a
    /// byte order swapped somewhere cannot pass. The CDN would report such a
    /// file as stored and the phone would show a broken picture.
    #[test]
    fn encryption_matches_the_published_aes_vector() {
        let key = [
            0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d,
            0x0e, 0x0f,
        ];
        let plain = [
            0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd,
            0xee, 0xff,
        ];
        let out = encrypt(&plain, &key);
        // One block in, two out: the second is the pure padding block.
        assert_eq!(out.len(), 32);
        assert_eq!(hex(&out[..16]), "69c4e0d86a7b0430d8cdb78070b4c55a");
    }

    /// The declared digest is how the CDN tells a truncated arrival from a
    /// whole one, so it has to be the digest of what actually went up.
    #[test]
    fn the_declared_digest_is_the_files_own() {
        assert_eq!(md5_hex(b""), "d41d8cd98f00b204e9800998ecf8427e");
        assert_eq!(md5_hex(b"abc"), "900150983cd24fb0d6963f7d28e17f72");
    }

    /// The key travels in the message and nowhere else — WeChat's CDN never
    /// sees it. A body that lost it would leave the phone holding bytes it
    /// cannot read, which looks on the wire exactly like a successful send.
    #[test]
    fn the_media_body_carries_the_key_the_phone_needs_to_read_the_file() {
        let uploaded = Uploaded {
            query_param: "param-1".into(),
            aes_key: "a2V5".into(),
            raw_size: 100,
            stored_size: 112,
        };
        let picture = media_body(
            "user-1",
            "id-1",
            "ctx-1",
            Medium::Image,
            "shot.png",
            &uploaded,
        );
        let item = &picture["msg"]["item_list"][0];
        assert_eq!(item["type"], ITEM_IMAGE);
        assert_eq!(item["image_item"]["media"]["aes_key"], "a2V5");
        assert_eq!(
            item["image_item"]["media"]["encrypt_query_param"],
            "param-1"
        );
        assert_eq!(item["image_item"]["media"]["encrypt_type"], ENCRYPT_AES);
        // An image is measured as stored: padded and encrypted.
        assert_eq!(item["image_item"]["mid_size"], 112);
        assert_eq!(picture["msg"]["context_token"], "ctx-1");
        assert_eq!(picture["msg"]["to_user_id"], "user-1");

        let document = media_body(
            "user-1",
            "id-1",
            "ctx-1",
            Medium::File,
            "notes.pdf",
            &uploaded,
        );
        let item = &document["msg"]["item_list"][0];
        assert_eq!(item["type"], ITEM_FILE);
        assert_eq!(item["file_item"]["file_name"], "notes.pdf");
        // A document is measured as the person sees it, and as a string —
        // this is the number printed under the name on the phone.
        assert_eq!(item["file_item"]["len"], "100");
        assert_eq!(item["file_item"]["media"]["aes_key"], "a2V5");
    }

    /// A key drawn from the clock the way [`nonce`] is would be a silent
    /// downgrade of the only protection the bytes get.
    #[test]
    fn every_file_is_encrypted_under_a_key_of_its_own() {
        let keys: std::collections::HashSet<[u8; BLOCK]> = (0..64)
            .map(|_| random_key().expect("the OS has randomness"))
            .collect();
        assert_eq!(keys.len(), 64, "a repeated key is not a key");
    }

    /// A conversation nobody has opened has no token to answer into, and the
    /// bytes should not be uploaded before that is discovered — an upload is
    /// the expensive half and it would be thrown away.
    #[test]
    fn media_will_not_be_uploaded_into_a_conversation_that_has_no_token() {
        let error = send_media(
            &Account::default(),
            "   ",
            Medium::Image,
            "shot.png",
            b"bytes",
            &[],
        )
        .unwrap_err();
        assert!(
            format!("{error:#}").contains("say anything to this bot"),
            "{error:#}"
        );
    }

    #[test]
    fn a_capture_writes_the_raw_body_to_a_private_file() {
        let dir = std::env::temp_dir().join(format!(
            "muxloom-ilink-capture-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        let body = json!({
            "errcode": 0,
            "errmsg": "success",
            "msgs": [{ "item_list": [], "context_token": "token" }],
        });
        write_raw_capture(&dir, "updates", 1234, &body);
        let raw = std::fs::read_to_string(dir.join("ilink-updates-1234.json"))
            .expect("the capture file is written");
        assert_eq!(
            serde_json::from_str::<Value>(&raw).expect("the capture is valid JSON"),
            body
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(dir.join("ilink-updates-1234.json"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(
                mode & 0o777,
                0o600,
                "the capture holds a context token and stays private"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_endpoint_is_built_the_same_whether_or_not_the_host_ends_in_a_slash() {
        assert_eq!(
            endpoint("https://example.com/", "sendmessage"),
            "https://example.com/ilink/bot/sendmessage"
        );
        assert_eq!(
            endpoint("https://example.com", "sendmessage"),
            "https://example.com/ilink/bot/sendmessage"
        );
    }

    #[test]
    fn what_a_person_attached_is_read_off_the_message_they_attached_it_to() {
        let message = json!({ "item_list": [
            { "type": 1, "text_item": { "text": "看这个" } },
            { "type": 2, "image_item": {
                "aeskey": "000102030405060708090a0b0c0d0e0f",
                "media": { "encrypt_query_param": "handle-1", "aes_key": "unused" },
            } },
            { "type": 4, "file_item": {
                "file_name": "季度报表.xlsx",
                "media": { "encrypt_query_param": "handle-2", "aes_key": "AAECAwQFBgcICQoLDA0ODw==" },
            } },
        ] });
        let found = enclosures(&message);
        assert_eq!(found.len(), 2, "{found:?}");
        // A picture is named by its bytes later, so it carries no name now —
        // and its key is the one beside the item, not the one under `media`,
        // which is the pair WeChat's own client disagrees about.
        assert_eq!(found[0].medium, Medium::Image);
        assert_eq!(found[0].name, "");
        assert_eq!(found[0].key, "000102030405060708090a0b0c0d0e0f");
        assert_eq!(found[1].medium, Medium::File);
        assert_eq!(found[1].name, "季度报表.xlsx");
        assert_eq!(found[1].query_param, "handle-2");
        // Both spellings of that one key are the same sixteen bytes.
        assert_eq!(
            parse_key(&found[0].key).unwrap(),
            parse_key(&found[1].key).unwrap()
        );
        // A voice note is left out on purpose: silk is a file nobody here can
        // open, and `missing` says so in words instead.
        assert!(enclosures(&json!({ "item_list": [{ "type": 3, "voice_item": {} }] })).is_empty());
        // Nothing to fetch it by is nothing to offer.
        assert!(enclosures(&json!({ "item_list": [{ "type": 2, "image_item": {} }] })).is_empty());
    }

    #[test]
    fn a_message_that_is_only_a_picture_is_still_a_message() {
        // It used to take words to survive this, which meant a photograph sent
        // with nothing typed alongside it reached the machine as silence.
        let said = parse_said(&[json!({
            "message_type": 1,
            "message_id": 7,
            "create_time_ms": 4,
            "item_list": [{ "type": 2, "image_item": {
                "aeskey": "000102030405060708090a0b0c0d0e0f",
                "media": { "encrypt_query_param": "handle" },
            } }],
        })]);
        assert_eq!(said.len(), 1);
        assert_eq!(said[0].text, "");
        assert_eq!(said[0].media.len(), 1);
        // A message with neither words nor anything attached still goes.
        assert!(parse_said(&[json!({ "message_type": 1, "item_list": [] })]).is_empty());
    }

    #[test]
    fn one_key_is_read_the_same_however_the_message_spelled_it() {
        let raw = [
            0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d,
            0x0e, 0x0f,
        ];
        // Thirty-two hex characters as they stand.
        assert_eq!(parse_key(&hex(&raw)).unwrap(), raw);
        // Base64 of the sixteen bytes.
        assert_eq!(parse_key(&base64(&raw)).unwrap(), raw);
        // And base64 of those same thirty-two hex characters, which is the
        // spelling that looks like a key twice as long as it is.
        assert_eq!(parse_key(&base64(hex(&raw).as_bytes())).unwrap(), raw);
        // Anything else is refused rather than half-read into a wrong key.
        assert!(parse_key("").is_err());
        assert!(parse_key("not a key at all!").is_err());
        assert!(parse_key(&hex(&[0u8; 8])).is_err());
    }

    #[test]
    fn a_file_survives_the_round_trip_and_a_wrong_key_is_caught() {
        let key = [7u8; BLOCK];
        let plain = b"\x89PNG\r\n\x1a\n and then some bytes that do not divide evenly".to_vec();
        assert_eq!(decrypt(&encrypt(&plain, &key), &key).unwrap(), plain);
        // A file that is exactly whole blocks still comes back whole, which is
        // what the extra padding block is for.
        let round = vec![3u8; BLOCK * 2];
        assert_eq!(decrypt(&encrypt(&round, &key), &key).unwrap(), round);
        // The wrong key decrypts just as happily and gives back noise. Padding
        // is the only thing that catches it, and it has to.
        let wrong = decrypt(&encrypt(&plain, &key), &[8u8; BLOCK]);
        assert!(wrong.is_err(), "a wrong key must not produce a file");
        // Not whole blocks is not something the CDN was ever going to send.
        assert!(decrypt(b"short", &key).is_err());
        assert!(decrypt(b"", &key).is_err());
    }
}
