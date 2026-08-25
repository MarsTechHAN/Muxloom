//! What an agent CLI records about its own sessions.
//!
//! Codex, Claude Code and pi each keep a transcript of every session they run,
//! and each names the session in it. muxloom reads those files in four places
//! (the resume picker, the backup index, the session list, and the recap under
//! it), so the rules for reading one live here rather than in each reader.
//!
//! Nothing here talks to a session: it reads files the CLI wrote, on the
//! machine the CLI ran on, and the reading is bounded. Transcripts reach tens
//! of megabytes, so a scan looks at the end of a file, not the file.

use std::cmp::Reverse;
use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::Value;

use crate::model::AgentKind;

/// How much of a transcript's end is read. Both CLIs write one JSON object per
/// line and append forever, so the latest exchange is found by reading the end.
const TAIL_BYTES: u64 = 256 * 1024;
/// How much of the start is read to find out when the conversation began.
/// Claude Code writes its first timestamped line inside the first kilobyte;
/// this leaves room for a transcript that opens with something larger.
const HEAD_BYTES: u64 = 64 * 1024;
/// How many transcripts one scan opens, newest first. A folder accumulates
/// every conversation ever held in it; the ones a live session could be
/// writing are at the top of that list.
const MAX_SCANNED: usize = 64;
/// How much of a message is kept, matching what a screen-scraped recap keeps.
const MAX_MESSAGE_CHARS: usize = 180;
/// A session and the transcript it writes begin with the same keystroke, but
/// not in the same instant - the CLI has to start first, and a transcript can
/// be stamped a moment before muxloom records the launch. Anything inside this
/// much slack counts as "started together".
pub const START_GRACE_MS: u64 = 30_000;

/// One conversation as its CLI recorded it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativeThread {
    /// What the CLI knows it by, which is what `--resume` takes.
    pub id: String,
    pub path: PathBuf,
    /// The folder the CLI recorded for itself, normalized.
    pub cwd: String,
    /// When the CLI says the conversation began (epoch ms), or 0 when the
    /// transcript never said - such a thread is only ever claimed by id.
    pub started_at: u64,
    /// When the transcript last grew (epoch ms).
    pub updated_at: u64,
    /// The thread this one was resumed out of, for the CLIs that fork.
    pub forked_from: Option<String>,
    /// The name the CLI gave the conversation.
    pub title: Option<String>,
    /// The last thing the agent said in it.
    pub last_message: Option<String>,
}

/// Every thread in `cwd` that has been written to since `since` (epoch ms).
///
/// `since` only bounds the reading: a caller decides for itself which of these
/// belongs to which session, because a folder can hold several conversations
/// at once and some of them belong to nobody here.
pub fn threads_for(kind: AgentKind, cwd: &str, since: u64) -> Vec<NativeThread> {
    let Some(home) = home_dir() else {
        return Vec::new();
    };
    match kind {
        AgentKind::Claude => claude_threads(&home.join(".claude").join("projects"), cwd, since),
        AgentKind::Codex => codex_threads(&home.join(".codex"), cwd, since),
        AgentKind::Pi => pi_threads(&home.join(".pi").join("agent").join("sessions"), cwd, since),
        // The rest keep no transcript of their own; muxloom's history is all
        // there is to read.
        AgentKind::OpenCode | AgentKind::Terminal => Vec::new(),
    }
}

/// Read one transcript that is already spoken for. A session that knows which
/// file it is writing never rescans the folder for it.
pub fn reread(kind: AgentKind, path: &Path) -> Option<NativeThread> {
    let updated_at = last_written(path)?;
    match kind {
        AgentKind::Claude => claude_thread(path, updated_at),
        AgentKind::Codex => {
            let names = codex_names(&home_dir()?.join(".codex"));
            codex_thread(path, updated_at, &names)
        }
        AgentKind::Pi => pi_thread(path, updated_at),
        AgentKind::OpenCode | AgentKind::Terminal => None,
    }
}

/// The thread a launch was told to reopen, read out of the command line the
/// daemon is about to run: `claude --resume <id>`, `codex resume <id>`,
/// `pi --session <id>`.
pub fn resume_seed(kind: AgentKind, args: &[String]) -> Option<String> {
    let flag = match kind {
        AgentKind::Claude => "--resume",
        AgentKind::Codex => "resume",
        AgentKind::Pi => "--session",
        AgentKind::OpenCode | AgentKind::Terminal => return None,
    };
    // `--flag=value` is only a form a flag has; Codex's is a subcommand.
    let joined = flag.starts_with("--").then(|| format!("{flag}="));
    for (index, argument) in args.iter().enumerate() {
        if let Some(id) = joined
            .as_deref()
            .and_then(|joined| argument.strip_prefix(joined))
        {
            return (!id.is_empty()).then(|| id.to_string());
        }
        if argument == flag {
            let id = args.get(index + 1)?;
            return (!id.starts_with('-')).then(|| id.clone());
        }
    }
    None
}

/// What the daemon knows about one session when it asks which transcript is
/// its own.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionFacts {
    /// When it was launched, in milliseconds - the clock every transcript
    /// here keeps. A session records its own launch in seconds, so whoever
    /// fills this in has to convert.
    pub created_at: u64,
    /// The thread the launch was told to reopen.
    pub seed: Option<String>,
    /// The thread it is already reading, or was reading before a restart.
    pub claimed: Option<String>,
    /// Threads it has been moved off and must not drift back onto.
    pub abandoned: Vec<String>,
}

/// Which thread each session is writing, as an index into `threads`.
///
/// Several agents of the same kind can be running in one folder, and the
/// folder holds the transcripts of every conversation ever held there, so this
/// is a matching problem rather than a lookup. In order of how much it is
/// worth trusting:
///
/// 1. A session already on a thread stays on it. Rematching every round would
///    let a newly started sibling take an older session's conversation away.
/// 2. A session launched to resume a thread gets that thread, or - for a CLI
///    that opens a fresh file when it resumes - the newest fork descended from
///    it.
/// 3. Whatever is left is matched by when it began: the session and the
///    transcript that started closest together are paired off first, then the
///    next closest, and so on. A transcript that began before every session
///    here belongs to an agent muxloom did not launch, and is left alone.
pub fn assign_threads(sessions: &[SessionFacts], threads: &[NativeThread]) -> Vec<Option<usize>> {
    let mut picks = vec![None; sessions.len()];
    let mut taken = vec![false; threads.len()];

    for (index, session) in sessions.iter().enumerate() {
        let Some(claimed) = session.claimed.as_deref() else {
            continue;
        };
        if session.abandoned.iter().any(|id| id == claimed) {
            continue;
        }
        if let Some(found) = threads.iter().position(|thread| thread.id == claimed)
            && !taken[found]
        {
            picks[index] = Some(found);
            taken[found] = true;
        }
    }

    for (index, session) in sessions.iter().enumerate() {
        if picks[index].is_some() {
            continue;
        }
        let Some(seed) = session.seed.as_deref() else {
            continue;
        };
        if let Some(found) = newest_descendant(threads, seed, &taken, &session.abandoned) {
            picks[index] = Some(found);
            taken[found] = true;
        }
    }

    // Every pairing still allowed, nearest in time first. Two agents started
    // seconds apart are only told apart by which transcript appeared closest
    // to which launch.
    let mut pairings = Vec::new();
    for (index, session) in sessions.iter().enumerate() {
        if picks[index].is_some() {
            continue;
        }
        for (thread_index, thread) in threads.iter().enumerate() {
            if taken[thread_index]
                // A transcript that never said when it began can only be
                // recognized by its id.
                || thread.started_at == 0
                || session.abandoned.contains(&thread.id)
                || session.created_at > thread.started_at.saturating_add(START_GRACE_MS)
            {
                continue;
            }
            pairings.push((
                thread.started_at.abs_diff(session.created_at),
                index,
                thread_index,
            ));
        }
    }
    pairings.sort_unstable();
    for (_, index, thread_index) in pairings {
        if picks[index].is_none() && !taken[thread_index] {
            picks[index] = Some(thread_index);
            taken[thread_index] = true;
        }
    }
    picks
}

/// The furthest fork descended from `seed`, or `seed` itself. Codex opens a
/// new rollout when it resumes one, so the thread a session was told to reopen
/// is an ancestor of the thread it is now writing.
fn newest_descendant(
    threads: &[NativeThread],
    seed: &str,
    taken: &[bool],
    abandoned: &[String],
) -> Option<usize> {
    let mut family = vec![seed.to_string()];
    let mut index = 0;
    while index < family.len() {
        let parent = family[index].clone();
        for thread in threads {
            if thread.forked_from.as_deref() == Some(parent.as_str())
                && !family.contains(&thread.id)
            {
                family.push(thread.id.clone());
            }
        }
        index += 1;
    }
    threads
        .iter()
        .enumerate()
        .filter(|(index, thread)| {
            !taken[*index] && family.contains(&thread.id) && !abandoned.contains(&thread.id)
        })
        .max_by_key(|(_, thread)| (thread.started_at, thread.updated_at))
        .map(|(index, _)| index)
}

/// Whether a message filed under the person's own role is something a person
/// actually said.
///
/// A runtime files a great deal besides. A slash command and whatever it
/// printed, the caveat pinned in front of a local command, the environment it
/// was given, a note that a turn was interrupted, the preamble of a
/// conversation carried over from one that ran out of room - all of them are
/// written down as the user speaking, and the first of them is very often the
/// first thing in the file. Take one for the name of a conversation and every
/// conversation ends up called the same thing, which is how a whole folder of
/// them came to be listed as `<local-command-caveat>Caveat: The messages
/// below...`.
///
/// Machinery gives itself away by its shape: it opens with a tag, or it is a
/// bracketed note and nothing else. The rest is a short list of the sentences
/// the runtimes write in prose. Where this guesses wrong the cost is only that
/// a reader falls through to the next thing said, which is why it leans toward
/// saying no.
pub fn is_spoken(text: &str) -> bool {
    let text = text.trim();
    if text.is_empty() || opens_with_a_tag(text) {
        return false;
    }
    if text.starts_with('[') && text.ends_with(']') {
        return false;
    }
    const WRITTEN_BY_THE_RUNTIME: [&str; 3] = [
        "caveat: the messages below were generated by the user",
        "this session is being continued from a previous conversation",
        "please continue the conversation from where we left it off",
    ];
    // However the runtime broke the line, the words are the same.
    let opening = text
        .split_whitespace()
        .take(12)
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase();
    !WRITTEN_BY_THE_RUNTIME
        .iter()
        .any(|preamble| opening.starts_with(preamble))
}

/// `<local-command-stdout>`, `<environment_context>`, `<system-reminder>`: a
/// tag name and nothing exotic in it. A person who really does open with
/// `<div>` loses nothing but the chance to name a conversation after it.
fn opens_with_a_tag(text: &str) -> bool {
    let Some(rest) = text.strip_prefix('<') else {
        return false;
    };
    let Some(end) = rest.find('>') else {
        return false;
    };
    let name = &rest[..end];
    !name.is_empty()
        && name
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'))
}

/// The name Claude Code gives a session today.
///
/// It is written on a line of its own and rewritten as the conversation goes
/// on, so a reader wants the *last* one in the file, not the first.
pub fn claude_ai_title(value: &Value) -> Option<&str> {
    (value.get("type").and_then(Value::as_str) == Some("ai-title"))
        .then(|| value.get("aiTitle").and_then(Value::as_str))
        .flatten()
        .map(str::trim)
        .filter(|title| !title.is_empty())
}

/// The name older Claude Code builds gave a session: a compaction summary, or
/// a title the user typed. Written once, so the first one found is the one.
pub fn claude_legacy_title(value: &Value) -> Option<&str> {
    value
        .get("summary")
        .and_then(Value::as_str)
        .or_else(|| value.get("customTitle").and_then(Value::as_str))
        .map(str::trim)
        .filter(|title| !title.is_empty())
}

fn claude_threads(projects: &Path, cwd: &str, since: u64) -> Vec<NativeThread> {
    let cwd = normalize_path(cwd);
    let folder = projects.join(claude_project_slug(&cwd));
    recent_files(&folder, since)
        .into_iter()
        .filter_map(|(path, updated_at)| claude_thread(&path, updated_at))
        .filter(|thread| thread.cwd == cwd)
        .collect()
}

/// Claude Code names a project folder after the path, with every character
/// that is not a letter or a digit replaced by a dash. That is lossy - two
/// folders can produce one name - so this only says where to look, and what
/// the transcript itself says about its cwd decides.
fn claude_project_slug(cwd: &str) -> String {
    cwd.chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() {
                character
            } else {
                '-'
            }
        })
        .collect()
}

fn claude_thread(path: &Path, updated_at: u64) -> Option<NativeThread> {
    let mut id = path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .map(str::to_string);
    let mut cwd = None;
    let mut started_at = 0;
    for value in read_head(path, HEAD_BYTES)?.lines().filter_map(parse_line) {
        if cwd.is_none() {
            cwd = value.get("cwd").and_then(Value::as_str).map(normalize_path);
        }
        if started_at == 0 {
            started_at = value
                .get("timestamp")
                .and_then(Value::as_str)
                .and_then(parse_timestamp)
                .unwrap_or(0);
        }
        if cwd.is_some() && started_at != 0 {
            break;
        }
    }

    let mut title = None;
    let mut legacy_title = None;
    let mut last_message = None;
    for value in read_tail(path, TAIL_BYTES)?.lines().filter_map(parse_line) {
        if id.is_none() {
            id = value
                .get("sessionId")
                .and_then(Value::as_str)
                .map(str::to_string);
        }
        if cwd.is_none() {
            cwd = value.get("cwd").and_then(Value::as_str).map(normalize_path);
        }
        if let Some(named) = claude_ai_title(&value) {
            title = clean_message(named);
        }
        if legacy_title.is_none() {
            legacy_title = claude_legacy_title(&value).and_then(clean_message);
        }
        if let Some(said) = claude_assistant_text(&value) {
            last_message = Some(said);
        }
    }

    Some(NativeThread {
        id: id?,
        path: path.to_path_buf(),
        cwd: cwd.unwrap_or_default(),
        started_at,
        updated_at,
        // Claude Code keeps writing the same file when it resumes one, so a
        // transcript here is never a fork of another.
        forked_from: None,
        title: title.or(legacy_title),
        last_message,
    })
}

/// What the agent itself said on a transcript line, if that is what it is. A
/// subagent's answer goes to the agent that asked for it, not to the person
/// reading the session list, so a sidechain line says nothing here.
fn claude_assistant_text(value: &Value) -> Option<String> {
    if value.get("type").and_then(Value::as_str) != Some("assistant")
        || value.get("isSidechain").and_then(Value::as_bool) == Some(true)
    {
        return None;
    }
    let blocks = value
        .get("message")
        .and_then(|message| message.get("content"))
        .and_then(Value::as_array)?;
    blocks
        .iter()
        .filter(|block| block.get("type").and_then(Value::as_str) == Some("text"))
        .filter_map(|block| block.get("text").and_then(Value::as_str))
        .filter_map(clean_message)
        .next_back()
}

fn codex_threads(codex_home: &Path, cwd: &str, since: u64) -> Vec<NativeThread> {
    let cwd = normalize_path(cwd);
    let names = codex_names(codex_home);
    let mut files = Vec::new();
    for day in codex_day_folders(&codex_home.join("sessions"), since) {
        files.extend(recent_files(&day, since));
    }
    files.sort_by_key(|(_, updated_at)| Reverse(*updated_at));
    files.truncate(MAX_SCANNED);
    files
        .into_iter()
        .filter_map(|(path, updated_at)| codex_thread(&path, updated_at, &names))
        .filter(|thread| thread.cwd == cwd)
        .collect()
}

/// Codex files its rollouts under `sessions/<year>/<month>/<day>`. Creating
/// one stamps the day folder it lands in, so a day untouched since `since`
/// cannot hold a thread that started then - but a year or a month folder is
/// only stamped when the folder below it is made, so the day is the only level
/// worth testing.
fn codex_day_folders(sessions: &Path, since: u64) -> Vec<PathBuf> {
    let mut days = Vec::new();
    for year in sub_folders(sessions) {
        for month in sub_folders(&year) {
            for day in sub_folders(&month) {
                if last_written(&day).is_some_and(|stamped| stamped >= since) {
                    days.push(day);
                }
            }
        }
    }
    days
}

/// The names Codex has given its threads, by thread id. It keeps them in one
/// small file beside the rollouts rather than in the rollouts themselves.
fn codex_names(codex_home: &Path) -> HashMap<String, String> {
    let mut names = HashMap::new();
    let Ok(index) = fs::read_to_string(codex_home.join("session_index.jsonl")) else {
        return names;
    };
    for value in index.lines().filter_map(parse_line) {
        if let (Some(id), Some(name)) = (
            value.get("id").and_then(Value::as_str),
            value.get("thread_name").and_then(Value::as_str),
        ) && let Some(name) = clean_message(name)
        {
            names.insert(id.to_string(), name);
        }
    }
    names
}

fn codex_thread(
    path: &Path,
    updated_at: u64,
    names: &HashMap<String, String>,
) -> Option<NativeThread> {
    let head = read_head(path, HEAD_BYTES)?;
    let meta = head
        .lines()
        .filter_map(parse_line)
        .find(|value| value.get("type").and_then(Value::as_str) == Some("session_meta"))?;
    let payload = meta.get("payload")?;
    // A subagent writes its own rollout in the same folder. It belongs to the
    // thread that spawned it, not to a session of its own.
    if payload
        .get("source")
        .and_then(|source| source.get("subagent"))
        .is_some()
    {
        return None;
    }
    let id = payload.get("id").and_then(Value::as_str)?.to_string();
    let cwd = payload
        .get("cwd")
        .and_then(Value::as_str)
        .map(normalize_path)
        .unwrap_or_default();
    let started_at = payload
        .get("timestamp")
        .and_then(Value::as_str)
        .or_else(|| meta.get("timestamp").and_then(Value::as_str))
        .and_then(parse_timestamp)
        .unwrap_or(0);
    let forked_from = payload
        .get("forked_from_id")
        .and_then(Value::as_str)
        .map(str::to_string);

    let mut last_message = None;
    for value in read_tail(path, TAIL_BYTES)?.lines().filter_map(parse_line) {
        if let Some(said) = codex_agent_text(&value) {
            last_message = Some(said);
        }
    }

    Some(NativeThread {
        title: names.get(&id).cloned(),
        id,
        path: path.to_path_buf(),
        cwd,
        started_at,
        updated_at,
        forked_from,
        last_message,
    })
}

/// What the agent itself said on a rollout line. A subagent's activity is
/// wrapped in an event of its own and never reads as this.
fn codex_agent_text(value: &Value) -> Option<String> {
    if value.get("type").and_then(Value::as_str) != Some("event_msg") {
        return None;
    }
    let payload = value.get("payload")?;
    if payload.get("type").and_then(Value::as_str) != Some("agent_message") {
        return None;
    }
    clean_message(payload.get("message").and_then(Value::as_str)?)
}

fn pi_threads(sessions: &Path, cwd: &str, since: u64) -> Vec<NativeThread> {
    let cwd = normalize_path(cwd);
    let folder = sessions.join(pi_session_slug(&cwd));
    recent_files(&folder, since)
        .into_iter()
        .filter_map(|(path, updated_at)| pi_thread(&path, updated_at))
        .filter(|thread| thread.cwd == cwd)
        .collect()
}

/// pi keeps one folder per working directory, named after the path itself: the
/// leading separator dropped, every separator and colon after it turned into a
/// dash, and the whole wrapped in a pair of dashes. Lossy in the same way
/// Claude Code's is, so this only says where to look, and what the transcript
/// says about its own cwd decides.
fn pi_session_slug(cwd: &str) -> String {
    let body: String = cwd
        .strip_prefix(['/', '\\'])
        .unwrap_or(cwd)
        .chars()
        .map(|character| match character {
            '/' | '\\' | ':' => '-',
            other => other,
        })
        .collect();
    format!("--{body}--")
}

/// The name pi was given for a conversation. It is a line of its own that can
/// be written again whenever the name changes, so the last one in the file is
/// the one that describes it now.
pub fn pi_session_name(value: &Value) -> Option<&str> {
    (value.get("type").and_then(Value::as_str) == Some("session_info"))
        .then(|| value.get("name").and_then(Value::as_str))
        .flatten()
        .map(str::trim)
        .filter(|name| !name.is_empty())
}

fn pi_thread(path: &Path, updated_at: u64) -> Option<NativeThread> {
    let head = read_head(path, HEAD_BYTES)?;
    let header = head
        .lines()
        .filter_map(parse_line)
        .find(|value| value.get("type").and_then(Value::as_str) == Some("session"))?;
    let id = header.get("id").and_then(Value::as_str)?.to_string();
    let cwd = header
        .get("cwd")
        .and_then(Value::as_str)
        .map(normalize_path)
        .unwrap_or_default();
    let started_at = header
        .get("timestamp")
        .and_then(Value::as_str)
        .and_then(parse_timestamp)
        .unwrap_or(0);
    // pi goes on writing the same file when it reopens one; a transcript here
    // is a fork only when it was asked for as one.
    let forked_from = header
        .get("parentSession")
        .and_then(Value::as_str)
        .map(str::to_string);

    // A name given at the start sits in the head of a transcript long enough
    // that the tail no longer reaches it, so both ends are read for one.
    let mut title = head
        .lines()
        .filter_map(parse_line)
        .filter_map(|value| pi_session_name(&value).and_then(clean_message))
        .next_back();
    let mut last_message = None;
    for value in read_tail(path, TAIL_BYTES)?.lines().filter_map(parse_line) {
        if let Some(named) = pi_session_name(&value).and_then(clean_message) {
            title = Some(named);
        }
        if let Some(said) = pi_agent_text(&value) {
            last_message = Some(said);
        }
    }

    Some(NativeThread {
        id,
        path: path.to_path_buf(),
        cwd,
        started_at,
        updated_at,
        forked_from,
        title,
        last_message,
    })
}

/// What the agent itself said on a transcript line. pi files a tool's answer
/// as a message too, under a role of its own, so the role is what tells the
/// two apart; a tool call inside an answer is a block without text.
fn pi_agent_text(value: &Value) -> Option<String> {
    if value.get("type").and_then(Value::as_str) != Some("message") {
        return None;
    }
    let message = value.get("message")?;
    if message.get("role").and_then(Value::as_str) != Some("assistant") {
        return None;
    }
    message
        .get("content")?
        .as_array()?
        .iter()
        .filter(|block| block.get("type").and_then(Value::as_str) == Some("text"))
        .filter_map(|block| block.get("text").and_then(Value::as_str))
        .filter_map(clean_message)
        .next_back()
}

/// The `*.jsonl` files in a folder that have grown since `since`, newest
/// first and bounded: a folder holds every conversation ever held there.
fn recent_files(folder: &Path, since: u64) -> Vec<(PathBuf, u64)> {
    let Ok(entries) = fs::read_dir(folder) else {
        return Vec::new();
    };
    let mut files: Vec<(PathBuf, u64)> = entries
        .flatten()
        .filter(|entry| entry.path().extension().is_some_and(|kind| kind == "jsonl"))
        .filter_map(|entry| {
            let stamped = entry
                .metadata()
                .ok()
                .and_then(|metadata| metadata.modified().ok())
                .and_then(as_epoch_ms)?;
            (stamped >= since).then(|| (entry.path(), stamped))
        })
        .collect();
    files.sort_by_key(|(_, updated_at)| Reverse(*updated_at));
    files.truncate(MAX_SCANNED);
    files
}

fn sub_folders(folder: &Path) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(folder) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
        .map(|entry| entry.path())
        .collect()
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|home| !home.as_os_str().is_empty())
}

/// When a file was last written to (epoch ms). A session that already knows
/// which transcript is its own asks this before reading it again: a
/// conversation nobody has added to has nothing new to say, and one that has
/// stopped growing under a session that is still talking is no longer the
/// conversation that session is having.
pub fn last_written(path: &Path) -> Option<u64> {
    as_epoch_ms(fs::metadata(path).ok()?.modified().ok()?)
}

fn as_epoch_ms(time: SystemTime) -> Option<u64> {
    let elapsed = time.duration_since(UNIX_EPOCH).ok()?;
    Some(elapsed.as_millis().min(u128::from(u64::MAX)) as u64)
}

/// The end of a file as text, minus the line the read landed in the middle of.
fn read_tail(path: &Path, limit: u64) -> Option<String> {
    let mut file = File::open(path).ok()?;
    let length = file.metadata().ok()?.len();
    let start = length.saturating_sub(limit);
    file.seek(SeekFrom::Start(start)).ok()?;
    let mut bytes = Vec::with_capacity((length - start) as usize);
    file.take(limit).read_to_end(&mut bytes).ok()?;
    let text = String::from_utf8_lossy(&bytes).into_owned();
    if start == 0 {
        return Some(text);
    }
    Some(match text.split_once('\n') {
        Some((_, rest)) => rest.to_string(),
        None => String::new(),
    })
}

/// The start of a file as text, minus the line the read stopped in the middle
/// of.
fn read_head(path: &Path, limit: u64) -> Option<String> {
    let mut bytes = Vec::new();
    File::open(path)
        .ok()?
        .take(limit)
        .read_to_end(&mut bytes)
        .ok()?;
    let text = String::from_utf8_lossy(&bytes).into_owned();
    if (bytes.len() as u64) < limit {
        return Some(text);
    }
    Some(match text.rsplit_once('\n') {
        Some((rest, _)) => rest.to_string(),
        None => String::new(),
    })
}

fn parse_line(line: &str) -> Option<Value> {
    serde_json::from_str(line).ok()
}

fn normalize_path(value: &str) -> String {
    if value == "/" {
        "/".into()
    } else {
        value.trim_end_matches('/').to_string()
    }
}

fn clean_message(value: &str) -> Option<String> {
    let flattened: String = value
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(MAX_MESSAGE_CHARS)
        .collect();
    (!flattened.is_empty()).then_some(flattened)
}

/// An ISO 8601 stamp as epoch milliseconds. Both CLIs write
/// `2026-08-19T03:48:05.620Z`; an offset is accepted because nothing promises
/// they always will.
fn parse_timestamp(value: &str) -> Option<u64> {
    let year: i64 = value.get(0..4)?.parse().ok()?;
    let month: i64 = value.get(5..7)?.parse().ok()?;
    let day: i64 = value.get(8..10)?.parse().ok()?;
    let hour: i64 = value.get(11..13)?.parse().ok()?;
    let minute: i64 = value.get(14..16)?.parse().ok()?;
    let second: i64 = value.get(17..19)?.parse().ok()?;
    let mut rest = value.get(19..)?;

    let mut millis = 0;
    if let Some(fraction) = rest.strip_prefix('.') {
        let digits: String = fraction
            .chars()
            .take_while(|character| character.is_ascii_digit())
            .collect();
        rest = &fraction[digits.len()..];
        let thousandths: String = digits.chars().chain("000".chars()).take(3).collect();
        millis = thousandths.parse().unwrap_or(0);
    }

    let mut offset_minutes = 0;
    if let Some(sign) = rest.chars().next()
        && matches!(sign, '+' | '-')
    {
        let hours: i64 = rest.get(1..3)?.parse().ok()?;
        let minutes: i64 = rest.get(4..6).unwrap_or("00").parse().unwrap_or(0);
        offset_minutes = (hours * 60 + minutes) * if sign == '-' { -1 } else { 1 };
    }

    let seconds = days_from_civil(year, month, day) * 86_400 + hour * 3_600 + minute * 60 + second
        - offset_minutes * 60;
    let stamped = seconds * 1_000 + millis;
    (stamped >= 0).then_some(stamped as u64)
}

/// Days between 1970-01-01 and a calendar date, by Howard Hinnant's civil
/// algorithm. muxloom carries no date library and needs this in exactly one
/// place: putting a transcript's own stamp on the same scale as a session's.
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = year - i64::from(month <= 2);
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let day_of_year = (153 * (month + if month > 2 { -3 } else { 9 }) + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;

    fn json(line: &str) -> Value {
        serde_json::from_str(line).expect("valid json")
    }

    /// Every rejected line here was taken off this machine, out of the
    /// transcripts of conversations actually held in it. The caveat opened a
    /// quarter of them.
    #[test]
    fn what_a_runtime_files_under_the_persons_name_is_not_the_person_talking() {
        for filed in [
            "<local-command-caveat>Caveat: The messages below were generated by the user while running local commands...</local-command-caveat>",
            "<command-name>/model</command-name> <command-message>model</command-message>",
            "<local-command-stdout>Set model to Opus 5 and saved it as your default</local-command-stdout>",
            "<environment_context> cwd: /work </environment_context>",
            "<system-reminder>the plan file exists</system-reminder>",
            "<bash-input>export HTTPS_PROXY=http://127.0.0.1:32722</bash-input>",
            "<task-notification> <task-id>begg5x4yf</task-id> </task-notification>",
            "[Request interrupted by user]",
            "[Request interrupted by user for tool use]",
            "[Image: original 2584x1190, displayed at 2000x921]",
            "Caveat: The messages below were generated by the user while running local commands.",
            "This session is being continued from a previous conversation that ran out of context.",
            "   ",
        ] {
            assert!(!is_spoken(filed), "machinery read as speech: {filed}");
        }

        for said in [
            "recap和文件名现在乱七八糟的，先修这个问题",
            "把本地的修改整理一下，打v0.4.7, commit&push吧",
            "Create a file called probe.txt containing the word hi, using the Write tool.",
            // An envelope around what another agent said still carries what it
            // said, and the bracket closes long before the end.
            "[muxloom] Message from claude \"Muxloom Agent接口\" on G3HMWLJP7: the pump is live",
            // The caveat is pinned in front of a message, not written instead
            // of one; a person who mentions it is still talking.
            "why does every session end up called Caveat: The messages below...?",
        ] {
            assert!(is_spoken(said), "speech read as machinery: {said}");
        }
    }

    fn scratch(name: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "muxloom-native-{name}-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir_all(&root).unwrap();
        root
    }

    fn thread(id: &str, started_at: u64) -> NativeThread {
        NativeThread {
            id: id.into(),
            path: PathBuf::from(format!("/tmp/{id}.jsonl")),
            cwd: "/work".into(),
            started_at,
            updated_at: started_at,
            forked_from: None,
            title: None,
            last_message: None,
        }
    }

    #[test]
    fn the_current_title_line_is_recognized_and_the_empty_one_ignored() {
        assert_eq!(
            claude_ai_title(&json(
                r#"{"type":"ai-title","aiTitle":" 优化触摸屏滑动体验 ","sessionId":"x"}"#
            )),
            Some("优化触摸屏滑动体验")
        );
        assert_eq!(
            claude_ai_title(&json(r#"{"type":"ai-title","aiTitle":"","sessionId":"x"}"#)),
            None
        );
        // A line that merely carries the field is not the title line.
        assert_eq!(
            claude_ai_title(&json(r#"{"type":"user","aiTitle":"not this"}"#)),
            None
        );
    }

    #[test]
    fn older_transcripts_still_give_up_their_title() {
        assert_eq!(
            claude_legacy_title(&json(r#"{"type":"summary","summary":"Ship the fix"}"#)),
            Some("Ship the fix")
        );
        assert_eq!(
            claude_legacy_title(&json(r#"{"customTitle":"Named by hand"}"#)),
            Some("Named by hand")
        );
        assert_eq!(claude_legacy_title(&json(r#"{"type":"user"}"#)), None);
    }

    #[test]
    fn an_iso_stamp_lands_on_the_same_scale_as_a_session() {
        assert_eq!(parse_timestamp("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(
            parse_timestamp("2026-08-19T03:48:05.620Z"),
            Some(1_787_111_285_620)
        );
        // Same instant, written from a machine that keeps local time.
        assert_eq!(
            parse_timestamp("2026-08-19T11:48:05.620+08:00"),
            parse_timestamp("2026-08-19T03:48:05.620Z")
        );
        assert_eq!(parse_timestamp("not a time"), None);
        assert_eq!(parse_timestamp(""), None);
    }

    #[test]
    fn a_folder_of_claude_transcripts_gives_up_its_names_and_last_answers() {
        let root = scratch("claude");
        let projects = root.join("projects");
        let folder = projects.join(claude_project_slug("/work/Terminal"));
        fs::create_dir_all(&folder).unwrap();
        fs::write(
            folder.join("aaa.jsonl"),
            concat!(
                r#"{"type":"mode","sessionId":"aaa"}"#, "\n",
                r#"{"type":"user","sessionId":"aaa","cwd":"/work/Terminal","timestamp":"2026-08-19T03:48:05.620Z"}"#, "\n",
                r#"{"type":"ai-title","aiTitle":"first guess at a name","sessionId":"aaa"}"#, "\n",
                r#"{"type":"assistant","sessionId":"aaa","message":{"content":[{"type":"text","text":"An early answer."}]}}"#, "\n",
                r#"{"type":"ai-title","aiTitle":"what it is really about","sessionId":"aaa"}"#, "\n",
                r#"{"type":"assistant","isSidechain":true,"sessionId":"aaa","message":{"content":[{"type":"text","text":"A subagent reporting back."}]}}"#, "\n",
                r#"{"type":"assistant","sessionId":"aaa","message":{"content":[{"type":"tool_use","name":"Read"},{"type":"text","text":"The keeper spawn is what fails."}]}}"#, "\n",
            ),
        )
        .unwrap();
        // A conversation held in a folder whose name happens to slug the same
        // way. Only what the transcript says about itself can tell them apart.
        fs::write(
            folder.join("bbb.jsonl"),
            concat!(
                r#"{"type":"user","sessionId":"bbb","cwd":"/work-Terminal","timestamp":"2026-08-19T03:50:00.000Z"}"#, "\n",
                r#"{"type":"assistant","sessionId":"bbb","message":{"content":[{"type":"text","text":"Somewhere else entirely."}]}}"#, "\n",
            ),
        )
        .unwrap();

        let threads = claude_threads(&projects, "/work/Terminal/", 0);
        assert_eq!(threads.len(), 1, "{threads:?}");
        let found = &threads[0];
        assert_eq!(found.id, "aaa");
        assert_eq!(found.cwd, "/work/Terminal");
        assert_eq!(found.title.as_deref(), Some("what it is really about"));
        assert_eq!(
            found.last_message.as_deref(),
            Some("The keeper spawn is what fails.")
        );
        assert_eq!(
            found.started_at,
            parse_timestamp("2026-08-19T03:48:05.620Z").unwrap()
        );
        assert!(found.updated_at > 0);

        // Nothing has been written since the scan's floor.
        assert!(claude_threads(&projects, "/work/Terminal", u64::MAX - 1).is_empty());
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn a_folder_of_pi_sessions_gives_up_its_names_forks_and_last_answers() {
        let root = scratch("pi");
        let sessions = root.join("sessions");
        let folder = sessions.join(pi_session_slug("/Users/me/Works/Terminal"));
        assert_eq!(
            folder.file_name().unwrap().to_str().unwrap(),
            "--Users-me-Works-Terminal--"
        );
        fs::create_dir_all(&folder).unwrap();
        fs::write(
            folder.join("2026-08-25T06-59-08-453Z_aaa.jsonl"),
            concat!(
                r#"{"type":"session","version":3,"id":"aaa","timestamp":"2026-08-25T06:59:08.453Z","cwd":"/Users/me/Works/Terminal","parentSession":"older"}"#, "\n",
                r#"{"type":"model_change","id":"m1","parentId":null,"timestamp":"2026-08-25T06:59:08.456Z","provider":"anthropic","modelId":"claude-opus-4-8"}"#, "\n",
                r#"{"type":"session_info","id":"n1","parentId":"m1","timestamp":"2026-08-25T06:59:20.000Z","name":"first guess at a name"}"#, "\n",
                r#"{"type":"message","id":"u1","parentId":"n1","timestamp":"2026-08-25T06:59:30.000Z","message":{"role":"user","content":[{"type":"text","text":"What's the star?"}]}}"#, "\n",
                r#"{"type":"message","id":"a1","parentId":"u1","timestamp":"2026-08-25T06:59:31.000Z","message":{"role":"assistant","content":[{"type":"text","text":"An early answer."}]}}"#, "\n",
                r#"{"type":"session_info","id":"n2","parentId":"a1","timestamp":"2026-08-25T07:00:00.000Z","name":"what it is really about"}"#, "\n",
                r#"{"type":"message","id":"t1","parentId":"n2","timestamp":"2026-08-25T07:00:01.000Z","message":{"role":"toolResult","content":[{"type":"text","text":"total 98729032"}]}}"#, "\n",
                r#"{"type":"message","id":"a2","parentId":"t1","timestamp":"2026-08-25T07:00:02.000Z","message":{"role":"assistant","content":[{"type":"text","text":"The keeper spawn is what fails."},{"type":"toolCall","id":"c1","name":"bash"}]}}"#, "\n",
            ),
        )
        .unwrap();
        // Another folder slugs to this same name; only what the transcript says
        // about its own cwd tells them apart.
        fs::write(
            folder.join("2026-08-25T07-02-08-614Z_bbb.jsonl"),
            concat!(
                r#"{"type":"session","version":3,"id":"bbb","timestamp":"2026-08-25T07:02:08.614Z","cwd":"/Users/me/Works-Terminal"}"#, "\n",
                r#"{"type":"message","id":"a1","parentId":null,"timestamp":"2026-08-25T07:02:09.000Z","message":{"role":"assistant","content":[{"type":"text","text":"Somewhere else entirely."}]}}"#, "\n",
            ),
        )
        .unwrap();

        let threads = pi_threads(&sessions, "/Users/me/Works/Terminal/", 0);
        assert_eq!(threads.len(), 1, "{threads:?}");
        let found = &threads[0];
        assert_eq!(found.id, "aaa");
        assert_eq!(found.cwd, "/Users/me/Works/Terminal");
        assert_eq!(found.title.as_deref(), Some("what it is really about"));
        assert_eq!(found.forked_from.as_deref(), Some("older"));
        assert_eq!(
            found.last_message.as_deref(),
            Some("The keeper spawn is what fails."),
            "a tool's answer is not the agent speaking"
        );
        assert_eq!(
            found.started_at,
            parse_timestamp("2026-08-25T06:59:08.453Z").unwrap()
        );
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn a_transcript_read_from_the_middle_of_a_line_still_reads() {
        let root = scratch("truncated");
        let path = root.join("ccc.jsonl");
        let filler = "x".repeat(4_000);
        fs::write(
            &path,
            format!(
                concat!(
                    r#"{{"type":"user","sessionId":"ccc","cwd":"/work","timestamp":"2026-08-19T03:48:05.620Z","note":"{filler}"}}"#,
                    "\n",
                    r#"{{"type":"assistant","sessionId":"ccc","message":{{"content":[{{"type":"text","text":"The tail is all that is read."}}]}}}}"#,
                    "\n"
                ),
                filler = filler
            ),
        )
        .unwrap();
        // Small enough that the read starts inside the first line.
        let tail = read_tail(&path, 512).unwrap();
        assert!(!tail.contains("xxxx"), "the partial line survived: {tail}");
        let found = claude_thread(&path, 1).unwrap();
        assert_eq!(
            found.last_message.as_deref(),
            Some("The tail is all that is read.")
        );
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn a_folder_of_codex_rollouts_gives_up_its_names_forks_and_last_answers() {
        let root = scratch("codex");
        let day = root.join("sessions").join("2026").join("08").join("19");
        fs::create_dir_all(&day).unwrap();
        fs::write(
            root.join("session_index.jsonl"),
            concat!(
                r#"{"id":"one","thread_name":"chase the flaky keeper","updated_at":"2026-08-19T04:00:00Z"}"#, "\n",
                r#"{"id":"two","thread_name":"chase the flaky keeper (2)","updated_at":"2026-08-19T05:00:00Z"}"#, "\n",
            ),
        )
        .unwrap();
        fs::write(
            day.join("rollout-2026-08-19T11-48-05-one.jsonl"),
            concat!(
                r#"{"type":"session_meta","timestamp":"2026-08-19T03:48:05.620Z","payload":{"id":"one","cwd":"/work","timestamp":"2026-08-19T03:48:05.620Z"}}"#, "\n",
                r#"{"type":"event_msg","payload":{"type":"agent_message","message":"The first thing it said."}}"#, "\n",
            ),
        )
        .unwrap();
        fs::write(
            day.join("rollout-2026-08-19T12-00-00-two.jsonl"),
            concat!(
                r#"{"type":"session_meta","payload":{"id":"two","cwd":"/work","timestamp":"2026-08-19T04:00:00.000Z","forked_from_id":"one"}}"#, "\n",
                r#"{"type":"event_msg","payload":{"type":"agent_reasoning","text":"thinking out loud"}}"#, "\n",
                r#"{"type":"event_msg","payload":{"type":"agent_message","message":"Picked up where it left off."}}"#, "\n",
            ),
        )
        .unwrap();
        // A subagent's rollout belongs to the thread that spawned it.
        fs::write(
            day.join("rollout-2026-08-19T12-05-00-helper.jsonl"),
            concat!(
                r#"{"type":"session_meta","payload":{"id":"helper","cwd":"/work","timestamp":"2026-08-19T04:05:00.000Z","source":{"subagent":{"parent":"two"}}}}"#, "\n",
            ),
        )
        .unwrap();

        let mut threads = codex_threads(&root, "/work", 0);
        threads.sort_by(|left, right| left.id.cmp(&right.id));
        assert_eq!(
            threads
                .iter()
                .map(|thread| thread.id.as_str())
                .collect::<Vec<_>>(),
            ["one", "two"]
        );
        assert_eq!(threads[0].title.as_deref(), Some("chase the flaky keeper"));
        assert_eq!(
            threads[0].last_message.as_deref(),
            Some("The first thing it said.")
        );
        assert_eq!(threads[1].forked_from.as_deref(), Some("one"));
        assert_eq!(
            threads[1].last_message.as_deref(),
            Some("Picked up where it left off.")
        );
        assert_eq!(
            threads[1].title.as_deref(),
            Some("chase the flaky keeper (2)")
        );
        assert_eq!(reread(AgentKind::Terminal, &threads[1].path), None);
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn a_launch_told_to_resume_says_which_thread_it_means() {
        assert_eq!(
            resume_seed(AgentKind::Claude, &["--resume".into(), "aaa".into()]),
            Some("aaa".into())
        );
        assert_eq!(
            resume_seed(
                AgentKind::Codex,
                &["--foo".into(), "resume".into(), "one".into()]
            ),
            Some("one".into())
        );
        assert_eq!(
            resume_seed(AgentKind::Pi, &["--session".into(), "aaa".into()]),
            Some("aaa".into())
        );
        assert_eq!(
            resume_seed(AgentKind::Pi, &["--session=aaa".into()]),
            Some("aaa".into())
        );
        assert_eq!(resume_seed(AgentKind::Claude, &["--resume".into()]), None);
        assert_eq!(
            resume_seed(AgentKind::Terminal, &["resume".into(), "one".into()]),
            None
        );
    }

    #[test]
    fn sessions_in_one_folder_each_get_their_own_thread() {
        let sessions = [
            SessionFacts {
                created_at: 500_000,
                ..Default::default()
            },
            SessionFacts {
                created_at: 560_000,
                ..Default::default()
            },
            SessionFacts {
                created_at: 620_000,
                ..Default::default()
            },
        ];
        // Listed newest first, as a scan returns them.
        let threads = [
            thread("third", 621_000),
            thread("second", 561_000),
            thread("first", 501_000),
            // Held here long before muxloom launched anything: somebody else's.
            thread("stranger", 1_000),
        ];
        assert_eq!(
            assign_threads(&sessions, &threads),
            [Some(2), Some(1), Some(0)]
        );
    }

    #[test]
    fn two_agents_started_seconds_apart_do_not_swap_conversations() {
        let sessions = [
            SessionFacts {
                created_at: 500_000,
                ..Default::default()
            },
            SessionFacts {
                created_at: 510_000,
                ..Default::default()
            },
        ];
        // Both transcripts fall inside the slack of both launches; only which
        // one appeared closest to which launch tells them apart.
        let threads = [thread("earlier", 502_000), thread("later", 512_000)];
        assert_eq!(assign_threads(&sessions, &threads), [Some(0), Some(1)]);
    }

    #[test]
    fn a_session_keeps_its_thread_when_a_sibling_starts_beside_it() {
        let threads = [thread("first", 2_000), thread("second", 61_000)];
        let sessions = [
            SessionFacts {
                created_at: 1_000,
                claimed: Some("first".into()),
                ..Default::default()
            },
            SessionFacts {
                created_at: 60_000,
                ..Default::default()
            },
        ];
        assert_eq!(assign_threads(&sessions, &threads), [Some(0), Some(1)]);
    }

    #[test]
    fn a_launch_that_resumed_follows_the_fork_its_cli_made() {
        let mut fork = thread("fork", 60_000);
        fork.forked_from = Some("origin".into());
        let mut refork = thread("refork", 90_000);
        refork.forked_from = Some("fork".into());
        let threads = [thread("origin", 10), fork, refork];
        let sessions = [SessionFacts {
            created_at: 59_000,
            seed: Some("origin".into()),
            ..Default::default()
        }];
        assert_eq!(assign_threads(&sessions, &threads), [Some(2)]);
    }

    #[test]
    fn a_session_that_started_over_moves_to_the_thread_it_started() {
        let threads = [thread("before", 1_000), thread("after", 500_000)];
        let sessions = [SessionFacts {
            created_at: 1_000,
            abandoned: vec!["before".into()],
            claimed: Some("before".into()),
            ..Default::default()
        }];
        assert_eq!(assign_threads(&sessions, &threads), [Some(1)]);
    }

    #[test]
    fn a_session_with_nothing_to_read_is_left_alone() {
        let sessions = [SessionFacts {
            created_at: 500_000,
            ..Default::default()
        }];
        assert_eq!(assign_threads(&sessions, &[thread("older", 1_000)]), [None]);
        assert_eq!(assign_threads(&sessions, &[]), [None]);
    }
}
