//! Local aggregation backup of agent conversation history.
//!
//! Every session's authoritative history lives on the machine that runs it:
//! muxloomd captures the raw terminal stream as `history/{id}.ansi`, and the
//! agent itself keeps a structured transcript (Codex rollout / Claude jsonl).
//! Neither is aggregated, capped, or searchable across machines.
//!
//! This module mirrors every session — running and archived, from every target
//! — into one self-contained store on the controller machine:
//!
//! ```text
//! <state>/backup/
//!   index.json                          # BackupRecord list (atomic write)
//!   blobs/<target>/<session>/
//!       capture.ansi.zst                # raw terminal stream, appended frames
//!       transcript.jsonl.zst            # agent-native structured conversation
//!       messages.zst                    # role+text extracted for search
//! ```
//!
//! Storage is deliberately file-based (no database): transparent, trivially
//! synced by external tools, and easy to prune. Blobs are compressed with zstd
//! — the terminal `.ansi` is extremely redundant (full-screen repaints) and
//! compresses ~50-350x. Incremental captures are stored as a sequence of
//! independent length-prefixed zstd frames so a sync only ever appends the new
//! delta; [`BackupStore::read_blob`] stitches the frames back into one stream.

use std::{
    collections::{HashMap, HashSet},
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    process::Command,
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{
    model::{AgentKind, LOCAL_TARGET_ID, RestoredTranscript, Target, Transport},
    runtime::{Runtime, is_temporary_session_id},
};

/// zstd level for at-rest compression. Level 12 already crushes the redundant
/// terminal/jsonl input (hundreds of x) while staying fast enough to compress a
/// multi-MB capture delta in well under a second.
const ZSTD_LEVEL: i32 = 12;

/// Blob file names within a session directory.
pub const CAPTURE_BLOB: &str = "capture.ansi.zst";
/// Whatever the runtime calls a conversation, mirrored byte for byte: the jsonl
/// transcript for the three runtimes that write one, and the single JSON
/// document OpenCode exports for a session, which keeps the old name because it
/// is the same slot — one blob per session, one path through retention, search
/// and restore, whatever shape the bytes inside it have.
pub const TRANSCRIPT_BLOB: &str = "transcript.jsonl.zst";
pub const MESSAGES_BLOB: &str = "messages.zst";

/// One backed-up session. Every field is `#[serde(default)]` so an index
/// written by an older build still loads as the schema grows.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
#[serde(default)]
pub struct BackupRecord {
    pub target_id: String,
    pub session_id: String,
    /// Agent kind as a string (`codex` | `claude` | `pi` | `opencode` |
    /// `terminal`).
    pub kind: String,
    /// Working directory the agent was launched in.
    pub cwd: String,
    pub created_at: u64,
    pub label: String,
    pub recap: String,
    pub title: String,
    pub dead: bool,
    pub archived: bool,
    /// Native agent transcript id + path, once resolved (empty until then).
    pub native_id: String,
    pub native_path: String,
    /// The transcript's last-updated marker at the last sync; lets us skip
    /// re-pulling (and, for remote targets, re-downloading) an unchanged file.
    pub native_updated_at: String,
    /// Incremental cursors so a sync only pulls new data. The capture cursor is
    /// the number of `.ansi` lines already mirrored (read_history is line
    /// addressed and works over both the local socket and SSH); the transcript
    /// cursor tracks the native file size.
    pub ansi_lines_synced: usize,
    pub jsonl_bytes_synced: u64,
    pub message_count: usize,
    /// Unix seconds of the last successful sync of this session.
    pub last_synced: u64,
}

impl BackupRecord {
    pub fn key(&self) -> (String, String) {
        (self.target_id.clone(), self.session_id.clone())
    }
}

/// A resolved network endpoint for a machine: `ssh -G`'s effective
/// `hostname`/`user`/`port`. The lowest-priority identity signal.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(default)]
pub struct MachineEndpoint {
    pub host: String,
    pub user: String,
    pub port: String,
}

/// A physical machine's stable identity, persisted in the backup index so that
/// the same box is recognised across ssh-alias churn. Sessions are partitioned
/// under [`MachineIdentity::key`] (assigned once, never changes). Matching a
/// target to a machine is by priority: ssh alias > host-key fingerprint >
/// endpoint (host+user+port). Every field is `#[serde(default)]` for forward
/// compatibility.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(default)]
pub struct MachineIdentity {
    /// Stable partition key = the alias first seen for this machine (or `local`).
    pub key: String,
    /// Every ssh alias observed to resolve to this machine.
    pub aliases: Vec<String>,
    /// Host-key SHA256 fingerprints (all key types) from the local known_hosts.
    pub fingerprints: Vec<String>,
    /// Resolved endpoints (host/user/port) observed for this machine.
    pub endpoints: Vec<MachineEndpoint>,
    /// Platform, filled lazily once (`uname` / local consts); empty until known.
    pub os: String,
    pub arch: String,
    pub first_seen: u64,
    pub last_seen: u64,
}

/// The on-disk index: a flat list of records (one per session; small) plus the
/// registry of known machines used to partition and de-alias those records.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct BackupIndex {
    #[serde(default)]
    pub records: Vec<BackupRecord>,
    #[serde(default)]
    pub machines: Vec<MachineIdentity>,
}

impl BackupIndex {
    /// Find a record's position by (partition, session).
    pub fn position(&self, target_id: &str, session_id: &str) -> Option<usize> {
        self.records
            .iter()
            .position(|record| record.target_id == target_id && record.session_id == session_id)
    }

    /// Insert or replace a record, keyed by (partition, session).
    pub fn upsert(&mut self, record: BackupRecord) {
        match self.position(&record.target_id, &record.session_id) {
            Some(index) => self.records[index] = record,
            None => self.records.push(record),
        }
    }

    /// Machine key for an ssh alias, or the alias itself if never registered.
    pub fn machine_key_for_alias(&self, alias: &str) -> String {
        self.machines
            .iter()
            .find(|machine| machine.aliases.iter().any(|known| known == alias))
            .map(|machine| machine.key.clone())
            .unwrap_or_else(|| alias.to_string())
    }
}

/// File-based backup store rooted at `<state>/backup/`.
pub struct BackupStore {
    root: PathBuf,
}

impl BackupStore {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    /// Default store next to the state file: `~/.local/state/muxloom/backup/`.
    pub fn default_root() -> PathBuf {
        crate::config::default_state_path()
            .parent()
            .map(|parent| parent.join("backup"))
            .unwrap_or_else(|| PathBuf::from("backup"))
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn index_path(&self) -> PathBuf {
        self.root.join("index.json")
    }

    pub fn session_dir(&self, target_id: &str, session_id: &str) -> PathBuf {
        self.root
            .join("blobs")
            .join(sanitize(target_id))
            .join(sanitize(session_id))
    }

    /// Load the index, returning an empty one if it does not exist yet.
    pub fn load_index(&self) -> Result<BackupIndex> {
        let path = self.index_path();
        if !path.exists() {
            return Ok(BackupIndex::default());
        }
        let text = fs::read_to_string(&path)
            .with_context(|| format!("failed to read backup index {}", path.display()))?;
        serde_json::from_str(&text)
            .with_context(|| format!("invalid backup index JSON in {}", path.display()))
    }

    /// Persist the index atomically (temp file in the same dir, then rename).
    pub fn save_index(&self, index: &BackupIndex) -> Result<()> {
        fs::create_dir_all(&self.root)
            .with_context(|| format!("failed to create {}", self.root.display()))?;
        let path = self.index_path();
        let tmp = path.with_extension("json.tmp");
        let text = serde_json::to_string_pretty(index)?;
        fs::write(&tmp, format!("{text}\n"))
            .with_context(|| format!("failed to write {}", tmp.display()))?;
        fs::rename(&tmp, &path)
            .with_context(|| format!("failed to rename into {}", path.display()))?;
        Ok(())
    }

    /// Append `chunk` to a session blob as its own length-prefixed zstd frame.
    /// Safe to call incrementally: prior frames are never rewritten.
    pub fn append_frame(
        &self,
        target_id: &str,
        session_id: &str,
        name: &str,
        chunk: &[u8],
    ) -> Result<()> {
        if chunk.is_empty() {
            return Ok(());
        }
        let dir = self.session_dir(target_id, session_id);
        fs::create_dir_all(&dir).with_context(|| format!("failed to create {}", dir.display()))?;
        let frame = encode_frame(chunk)?;
        let path = dir.join(name);
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .with_context(|| format!("failed to open {}", path.display()))?;
        file.write_all(&frame)
            .with_context(|| format!("failed to append to {}", path.display()))?;
        Ok(())
    }

    /// Replace a session blob with a single zstd frame (for whole-file blobs we
    /// re-pull when changed, e.g. the native transcript).
    pub fn write_blob(
        &self,
        target_id: &str,
        session_id: &str,
        name: &str,
        data: &[u8],
    ) -> Result<()> {
        let dir = self.session_dir(target_id, session_id);
        fs::create_dir_all(&dir).with_context(|| format!("failed to create {}", dir.display()))?;
        let frame = encode_frame(data)?;
        let path = dir.join(name);
        let tmp = path.with_extension("zst.tmp");
        fs::write(&tmp, &frame).with_context(|| format!("failed to write {}", tmp.display()))?;
        fs::rename(&tmp, &path)
            .with_context(|| format!("failed to rename into {}", path.display()))?;
        Ok(())
    }

    /// Read and decompress a session blob, stitching all appended frames.
    /// Returns an empty vec if the blob does not exist.
    pub fn read_blob(&self, target_id: &str, session_id: &str, name: &str) -> Result<Vec<u8>> {
        let path = self.session_dir(target_id, session_id).join(name);
        if !path.exists() {
            return Ok(Vec::new());
        }
        let bytes =
            fs::read(&path).with_context(|| format!("failed to read {}", path.display()))?;
        decode_frames(&bytes).with_context(|| format!("failed to zstd-decode {}", path.display()))
    }

    /// Read at most `max_bytes` of the *newest* decompressed output of a blob,
    /// leaving everything older compressed on disk. Returns the bytes and
    /// whether older output was left behind.
    ///
    /// A capture blob holds every row a session ever printed — tens of
    /// megabytes decompressed — while a pane shows a few hundred rows, so
    /// reading the whole thing to display the end of it is pure cost. Frames
    /// are walked back-to-front and only the ones the budget needs are
    /// decompressed; the result is then cut to `max_bytes` at a line boundary.
    pub fn read_blob_tail(
        &self,
        target_id: &str,
        session_id: &str,
        name: &str,
        max_bytes: usize,
    ) -> Result<(Vec<u8>, bool)> {
        let path = self.session_dir(target_id, session_id).join(name);
        if !path.exists() {
            return Ok((Vec::new(), false));
        }
        let bytes =
            fs::read(&path).with_context(|| format!("failed to read {}", path.display()))?;
        let frames = frame_spans(&bytes);
        let mut first = frames.len();
        let mut raw = 0usize;
        while first > 0 && raw < max_bytes {
            first -= 1;
            raw = raw.saturating_add(frames[first].2);
        }
        let mut out = Vec::with_capacity(raw.min(max_bytes.saturating_mul(2)));
        for (start, end, _) in &frames[first..] {
            out.extend_from_slice(
                &decode_frames(&bytes[*start..*end])
                    .with_context(|| format!("failed to zstd-decode {}", path.display()))?,
            );
        }
        let mut clipped = first > 0;
        if out.len() > max_bytes {
            let mut cut = out.len() - max_bytes;
            // Land on a line boundary so the first row shown is a whole row.
            if let Some(offset) = out[cut..].iter().position(|byte| *byte == b'\n') {
                cut += offset + 1;
            }
            out.drain(..cut);
            clipped = true;
        }
        Ok((out, clipped))
    }

    /// Compressed on-disk size of a session blob (0 if absent).
    pub fn blob_len(&self, target_id: &str, session_id: &str, name: &str) -> u64 {
        let path = self.session_dir(target_id, session_id).join(name);
        fs::metadata(&path).map(|meta| meta.len()).unwrap_or(0)
    }

    /// Remove a session blob (used by retention). Missing is not an error.
    pub fn remove_blob(&self, target_id: &str, session_id: &str, name: &str) -> Result<()> {
        let path = self.session_dir(target_id, session_id).join(name);
        match fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => {
                Err(error).with_context(|| format!("failed to remove {}", path.display()))
            }
        }
    }

    /// Cap the compressed capture blob to `max_bytes` by dropping whole frames
    /// from the front (the oldest terminal output), keeping at least the newest
    /// frame. Scrollback-style retention: the sync cursor is unaffected, so
    /// future appends still continue in order.
    pub fn retain_capture(&self, target_id: &str, session_id: &str, max_bytes: u64) -> Result<()> {
        if max_bytes == 0 {
            return Ok(());
        }
        let path = self.session_dir(target_id, session_id).join(CAPTURE_BLOB);
        let Ok(meta) = fs::metadata(&path) else {
            return Ok(());
        };
        if meta.len() <= max_bytes {
            return Ok(());
        }
        let bytes = fs::read(&path)?;
        let frames = frame_spans(&bytes);
        let mut total: usize = frames.iter().map(|(start, end, _)| end - start).sum();
        let mut drop_to = 0usize;
        while total > max_bytes as usize && drop_to + 1 < frames.len() {
            let (start, end, _) = frames[drop_to];
            total -= end - start;
            drop_to += 1;
        }
        if drop_to == 0 {
            return Ok(());
        }
        let keep_from = frames[drop_to].0;
        let path_tmp = path.with_extension("zst.tmp");
        fs::write(&path_tmp, &bytes[keep_from..])?;
        fs::rename(&path_tmp, &path)?;
        Ok(())
    }
}

/// Frame layout on disk: `[u64 LE raw_len][u64 LE comp_len][comp_bytes]`.
/// The raw length preallocates the decompress buffer; the compressed length
/// bounds the read so concatenated frames are unambiguous.
const FRAME_HEADER_LEN: usize = 16;

fn encode_frame(chunk: &[u8]) -> Result<Vec<u8>> {
    let compressed =
        zstd::bulk::compress(chunk, ZSTD_LEVEL).context("failed to zstd-compress backup chunk")?;
    let mut frame = Vec::with_capacity(FRAME_HEADER_LEN + compressed.len());
    frame.extend_from_slice(&(chunk.len() as u64).to_le_bytes());
    frame.extend_from_slice(&(compressed.len() as u64).to_le_bytes());
    frame.extend_from_slice(&compressed);
    Ok(frame)
}

/// The `(start, end, raw_len)` of each whole frame in a blob, read from the
/// headers alone — nothing is decompressed. A short trailing write is ignored,
/// same as [`decode_frames`].
fn frame_spans(bytes: &[u8]) -> Vec<(usize, usize, usize)> {
    let mut frames = Vec::new();
    let mut pos = 0usize;
    while pos + FRAME_HEADER_LEN <= bytes.len() {
        let raw_len = u64::from_le_bytes(bytes[pos..pos + 8].try_into().unwrap()) as usize;
        let comp_len = u64::from_le_bytes(bytes[pos + 8..pos + 16].try_into().unwrap()) as usize;
        let end = pos + FRAME_HEADER_LEN + comp_len;
        if end > bytes.len() {
            break;
        }
        frames.push((pos, end, raw_len));
        pos = end;
    }
    frames
}

fn decode_frames(bytes: &[u8]) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    let mut pos = 0usize;
    while pos < bytes.len() {
        if pos + FRAME_HEADER_LEN > bytes.len() {
            // A short trailing write (e.g. an interrupted append) leaves a
            // partial header; ignore it rather than fail the whole read.
            break;
        }
        let raw_len = u64::from_le_bytes(bytes[pos..pos + 8].try_into().unwrap()) as usize;
        let comp_len = u64::from_le_bytes(bytes[pos + 8..pos + 16].try_into().unwrap()) as usize;
        let start = pos + FRAME_HEADER_LEN;
        let end = start + comp_len;
        if end > bytes.len() {
            break; // truncated frame body; stop at the last whole frame
        }
        let plain = zstd::bulk::decompress(&bytes[start..end], raw_len)
            .context("failed to zstd-decompress backup frame")?;
        if plain.len() != raw_len {
            bail!(
                "backup frame length mismatch: expected {raw_len}, got {}",
                plain.len()
            );
        }
        out.extend_from_slice(&plain);
        pos = end;
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Sync: pull each target's sessions into the store.
// ---------------------------------------------------------------------------

/// A single conversation message extracted from a native transcript.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtractedMessage {
    pub role: String,
    pub text: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub ts: String,
}

/// What one sync pass touched, for the status line.
#[derive(Debug, Default, Clone)]
pub struct SyncSummary {
    pub sessions: usize,
    pub transcripts: usize,
    pub ansi_bytes: u64,
}

impl std::fmt::Display for SyncSummary {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "backed up {} session(s), {} transcript update(s), {} KiB capture",
            self.sessions,
            self.transcripts,
            self.ansi_bytes / 1024
        )
    }
}

/// Run one backup pass over `targets` into the default store, updating the
/// shared index. Errors on a single target are logged and skipped so one
/// offline machine does not abort the whole pass.
/// An identity probe for one target: the signals used to match it to a machine.
/// SSH-only fields are empty for the local target (which matches by alias only).
struct IdentityProbe {
    /// The ssh alias (`local` for the local target); "" only if unknowable.
    alias: String,
    /// Host-key SHA256 fingerprints from the local known_hosts (all key types).
    fingerprints: Vec<String>,
    /// Resolved endpoint from `ssh -G`, when available.
    endpoint: Option<MachineEndpoint>,
}

/// Parse `ssh -G <alias>` output into an endpoint. Keys are lowercased by ssh.
fn parse_ssh_config_g(text: &str) -> MachineEndpoint {
    let mut endpoint = MachineEndpoint::default();
    for line in text.lines() {
        let mut fields = line.split_whitespace();
        match (fields.next(), fields.next()) {
            (Some("hostname"), Some(value)) => endpoint.host = value.to_string(),
            (Some("user"), Some(value)) => endpoint.user = value.to_string(),
            (Some("port"), Some(value)) => endpoint.port = value.to_string(),
            _ => {}
        }
    }
    endpoint
}

/// Extract every `SHA256:...` fingerprint token from `ssh-keygen -lF` output,
/// de-duplicated. A host commonly has several keys (ed25519/rsa/ecdsa).
fn parse_known_host_fingerprints(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for token in text.split_whitespace() {
        if token.starts_with("SHA256:") {
            let token = token.to_string();
            if !out.contains(&token) {
                out.push(token);
            }
        }
    }
    out
}

/// Resolve an ssh alias to a `hostname/user/port` endpoint via `ssh -G` (local,
/// no connection). Returns None if ssh is missing or emits no hostname.
fn ssh_endpoint(alias: &str) -> Option<MachineEndpoint> {
    let output = Command::new("ssh").args(["-G", alias]).output().ok()?;
    if !output.status.success() {
        return None;
    }
    let endpoint = parse_ssh_config_g(&String::from_utf8_lossy(&output.stdout));
    (!endpoint.host.is_empty()).then_some(endpoint)
}

/// Read the host-key fingerprints already trusted for an endpoint from the
/// local known_hosts via `ssh-keygen -lF` (no network). Empty if not present
/// yet (e.g. first sync before the host is trusted) — matching then falls back
/// to the endpoint and self-heals next pass.
fn known_host_fingerprints(host: &str, port: &str) -> Vec<String> {
    let query = if port.is_empty() || port == "22" {
        host.to_string()
    } else {
        format!("[{host}]:{port}")
    };
    match Command::new("ssh-keygen").args(["-lF", &query]).output() {
        // ssh-keygen exits non-zero when the host is absent; that is not an error.
        Ok(output) => parse_known_host_fingerprints(&String::from_utf8_lossy(&output.stdout)),
        Err(_) => Vec::new(),
    }
}

/// Build the identity probe for a target using local commands only.
fn probe_identity(target: &Target) -> IdentityProbe {
    match &target.transport {
        Transport::Local => IdentityProbe {
            alias: LOCAL_TARGET_ID.to_string(),
            fingerprints: Vec::new(),
            endpoint: None,
        },
        Transport::Ssh { alias } => {
            let endpoint = ssh_endpoint(alias);
            let fingerprints = endpoint
                .as_ref()
                .map(|e| known_host_fingerprints(&e.host, &e.port))
                .unwrap_or_default();
            IdentityProbe {
                alias: alias.clone(),
                fingerprints,
                endpoint,
            }
        }
    }
}

/// Union the probe's signals into an existing machine record.
fn merge_identity(machine: &mut MachineIdentity, probe: &IdentityProbe, now: u64) {
    if !probe.alias.is_empty() && !machine.aliases.iter().any(|a| a == &probe.alias) {
        machine.aliases.push(probe.alias.clone());
    }
    for fingerprint in &probe.fingerprints {
        if !machine.fingerprints.contains(fingerprint) {
            machine.fingerprints.push(fingerprint.clone());
        }
    }
    if let Some(endpoint) = &probe.endpoint
        && !machine.endpoints.contains(endpoint)
    {
        machine.endpoints.push(endpoint.clone());
    }
    if machine.first_seen == 0 {
        machine.first_seen = now;
    }
    machine.last_seen = now;
}

/// Match a probe to a known machine by priority (alias > fingerprint > endpoint)
/// and merge, or register a new machine. Returns the stable partition key.
fn resolve_machine(machines: &mut Vec<MachineIdentity>, probe: &IdentityProbe, now: u64) -> String {
    let matched = machines
        .iter()
        .position(|m| !probe.alias.is_empty() && m.aliases.iter().any(|a| a == &probe.alias))
        .or_else(|| {
            machines.iter().position(|m| {
                probe
                    .fingerprints
                    .iter()
                    .any(|f| m.fingerprints.contains(f))
            })
        })
        .or_else(|| {
            probe
                .endpoint
                .as_ref()
                .and_then(|endpoint| machines.iter().position(|m| m.endpoints.contains(endpoint)))
        });
    match matched {
        Some(index) => {
            merge_identity(&mut machines[index], probe, now);
            machines[index].key.clone()
        }
        None => {
            let key = if !probe.alias.is_empty() {
                probe.alias.clone()
            } else if let Some(fingerprint) = probe.fingerprints.first() {
                fingerprint.clone()
            } else if let Some(endpoint) = &probe.endpoint {
                format!("{}@{}:{}", endpoint.user, endpoint.host, endpoint.port)
            } else {
                "unknown".to_string()
            };
            let mut machine = MachineIdentity {
                key: key.clone(),
                ..Default::default()
            };
            merge_identity(&mut machine, probe, now);
            machines.push(machine);
            key
        }
    }
}

/// Resolve a target to its stable machine partition key, updating the registry.
fn resolve_partition(machines: &mut Vec<MachineIdentity>, target: &Target, now: u64) -> String {
    let probe = probe_identity(target);
    resolve_machine(machines, &probe, now)
}

/// Fill in a machine's platform once (cheap `uname`, or local consts), so an
/// offline machine still shows what it is. No-op once known.
fn enrich_platform(index: &mut BackupIndex, runtime: &Runtime, target: &Target, partition: &str) {
    let already_known = index
        .machines
        .iter()
        .any(|m| m.key == partition && !m.os.is_empty());
    if already_known {
        return;
    }
    let platform = match target.transport {
        Transport::Local => Some((
            std::env::consts::OS.to_string(),
            std::env::consts::ARCH.to_string(),
        )),
        Transport::Ssh { .. } => runtime.probe_platform(target),
    };
    if let Some((os, arch)) = platform
        && let Some(machine) = index.machines.iter_mut().find(|m| m.key == partition)
    {
        machine.os = os;
        machine.arch = arch;
    }
}

/// Machine key for an ssh alias against the on-disk registry (or the alias if
/// unregistered). Used by the UI to compare machines, not just aliases.
pub fn machine_key_for_alias(store: &BackupStore, alias: &str) -> String {
    store
        .load_index()
        .map(|index| index.machine_key_for_alias(alias))
        .unwrap_or_else(|_| alias.to_string())
}

pub fn run_sync(
    runtime: &Runtime,
    targets: &[Target],
    include_ansi: bool,
    ansi_max_bytes: u64,
) -> Result<SyncSummary> {
    let store = BackupStore::new(BackupStore::default_root());
    let mut index = store.load_index()?;
    let mut summary = SyncSummary::default();
    for target in targets {
        match sync_target(
            runtime,
            &store,
            &mut index,
            target,
            include_ansi,
            ansi_max_bytes,
        ) {
            Ok(stats) => {
                summary.sessions += stats.sessions;
                summary.transcripts += stats.transcripts;
                summary.ansi_bytes += stats.ansi_bytes;
            }
            Err(error) => {
                crate::debug::log("backup", format!("sync {} failed: {error:#}", target.id));
            }
        }
    }
    store.save_index(&index)?;
    Ok(summary)
}

/// Back up every session (running + archived) on one target.
pub fn sync_target(
    runtime: &Runtime,
    store: &BackupStore,
    index: &mut BackupIndex,
    target: &Target,
    include_ansi: bool,
    ansi_max_bytes: u64,
) -> Result<SyncSummary> {
    let mut stats = SyncSummary::default();
    let is_local = matches!(target.transport, Transport::Local);
    // Canonicalise the target to a stable machine key, then partition all
    // storage/records under it (bridge calls below still use the real target).
    let partition = resolve_partition(&mut index.machines, target, now_unix());
    enrich_platform(index, runtime, target, &partition);
    let sessions = runtime
        .bridge_pool()
        .list_sessions(target)
        .with_context(|| format!("failed to list sessions on {}", target.id))?;

    // The daemon already worked out which transcript belongs to which session,
    // and it did it with facts the backup does not have: what each launch was
    // told to resume, what each session was reading before a restart, which
    // threads it has been moved off. Collect those matches before mirroring
    // anything so that a session the daemon did not match cannot walk off with
    // a transcript that is spoken for.
    let mut spoken_for: HashMap<String, String> = sessions
        .iter()
        .filter_map(|session| {
            session
                .thread
                .clone()
                .map(|thread| (thread, session.id.clone()))
        })
        .collect();

    for session in sessions {
        if session.temporary || is_temporary_session_id(&session.id) {
            continue;
        }
        let mut record = index
            .position(&partition, &session.id)
            .map(|position| index.records[position].clone())
            .unwrap_or_default();
        record.target_id = partition.clone();
        record.session_id = session.id.clone();
        record.kind = session.kind.clone();
        record.cwd = session.path.clone();
        record.created_at = session.created_at;
        record.label = session.label.clone();
        record.recap = session.recap.clone().unwrap_or_default();
        record.dead = session.dead;
        record.archived = session.archived;

        if include_ansi {
            if let Err(error) = sync_capture(
                runtime,
                store,
                target,
                &partition,
                &session.id,
                &mut record,
                &mut stats,
            ) {
                crate::debug::log(
                    "backup",
                    format!("capture {} on {} failed: {error:#}", session.id, target.id),
                );
            } else if let Err(error) = store.retain_capture(&partition, &session.id, ansi_max_bytes)
            {
                crate::debug::log("backup", format!("retain {} failed: {error:#}", session.id));
            }
        }

        // Only a runtime that keeps its own transcript on disk has something
        // for the backup to mirror; the rest live entirely in their capture.
        if let Ok(kind) = session.kind.parse::<AgentKind>()
            && kind.has_native_history()
            && let Err(error) = sync_transcript(
                runtime,
                store,
                target,
                &partition,
                is_local,
                kind,
                &session,
                &mut record,
                &mut spoken_for,
                &mut stats,
            )
        {
            crate::debug::log(
                "backup",
                format!(
                    "transcript {} on {} failed: {error:#}",
                    session.id, target.id
                ),
            );
        }

        record.last_synced = now_unix();
        index.upsert(record);
        stats.sessions += 1;
    }
    Ok(stats)
}

/// Number of `.ansi` lines to pull per `read_history` window. Bounds a single
/// transfer/frame; the common incremental delta is well under this so a sync is
/// one round trip. Kept modest because a full-screen agent's "lines" are large
/// redraw frames.
const CAPTURE_CHUNK_LINES: usize = 5_000;

/// On a session's first backup, only mirror this many recent `.ansi` lines
/// rather than backfilling the entire (possibly huge) pre-existing history. The
/// full structured conversation is still captured as jsonl, and retention would
/// trim old capture regardless.
const CAPTURE_BACKFILL_LINES: usize = 5_000;

/// Mirror the session's raw `.ansi` capture incrementally via `read_history`
/// (works over the local socket and SSH alike). Faithful raw bytes are appended
/// as new zstd frames; only the delta beyond `ansi_lines_synced` is transferred.
#[allow(clippy::too_many_arguments)]
fn sync_capture(
    runtime: &Runtime,
    store: &BackupStore,
    target: &Target,
    partition: &str,
    session_id: &str,
    record: &mut BackupRecord,
    stats: &mut SyncSummary,
) -> Result<()> {
    let bridges = runtime.bridge_pool();
    let total = bridges
        // The backup mirrors the append-only log itself, so it asks for raw
        // lines rather than rendered rows.
        .read_history(target, session_id.to_string(), 0, 1, false)?
        .total_lines;
    if total < record.ansi_lines_synced {
        // Source rotated/shrank → restart the capture blob.
        store.remove_blob(partition, session_id, CAPTURE_BLOB)?;
        record.ansi_lines_synced = 0;
    }
    if record.ansi_lines_synced == 0 {
        // First backup of this session: skip ancient history, mirror the tail.
        record.ansi_lines_synced = total.saturating_sub(CAPTURE_BACKFILL_LINES);
    }
    while record.ansi_lines_synced < total {
        let window = (total - record.ansi_lines_synced).min(CAPTURE_CHUNK_LINES);
        let offset = total - (record.ansi_lines_synced + window);
        let page = bridges.read_history(target, session_id.to_string(), offset, window, false)?;
        if page.bytes.is_empty() {
            break;
        }
        store.append_frame(partition, session_id, CAPTURE_BLOB, &page.bytes)?;
        stats.ansi_bytes += page.bytes.len() as u64;
        record.ansi_lines_synced += window;
    }
    Ok(())
}

/// Resolve and mirror the agent-native transcript (Codex rollout / Claude
/// jsonl / Pi jsonl). Local files are read directly; remote files are pulled
/// over the bridge; OpenCode, which has no file to copy, is asked for its
/// session instead. Skips work when the transcript's update marker is
/// unchanged.
#[allow(clippy::too_many_arguments)]
fn sync_transcript(
    runtime: &Runtime,
    store: &BackupStore,
    target: &Target,
    partition: &str,
    is_local: bool,
    kind: AgentKind,
    session: &crate::daemon_protocol::DaemonSession,
    record: &mut BackupRecord,
    spoken_for: &mut HashMap<String, String>,
    stats: &mut SyncSummary,
) -> Result<()> {
    let candidates = runtime.scan_resumes(target, kind, &session.path)?;
    let Some(candidate) = pick_native_candidate(&candidates, session, record, spoken_for) else {
        return Ok(());
    };
    spoken_for.insert(candidate.id.clone(), session.id.clone());
    // A pairing that changes was a pairing that was wrong: the mirrored blobs,
    // the message count and the title all describe the other conversation, so
    // they go and the new transcript is pulled whole.
    if !record.native_id.is_empty() && record.native_id != candidate.id {
        store.remove_blob(partition, &session.id, TRANSCRIPT_BLOB)?;
        store.remove_blob(partition, &session.id, MESSAGES_BLOB)?;
        record.native_updated_at.clear();
        record.jsonl_bytes_synced = 0;
        record.message_count = 0;
        record.title.clear();
    }
    record.native_id = candidate.id.clone();
    record.native_path = candidate.source_path.clone();
    // A name taken before muxloom could tell machinery from speech - the
    // caveat pinned in front of a local command, a slash command and what it
    // printed. It says nothing about the conversation, so take the guess again
    // off the messages already mirrored here rather than leave it standing
    // until a transcript nobody is adding to grows again.
    if !record.title.is_empty() && !crate::native_history::is_spoken(&record.title) {
        record.title = store
            .read_blob(partition, &session.id, MESSAGES_BLOB)
            .ok()
            .and_then(|raw| guess_title(&messages_from_jsonl(&raw)))
            .unwrap_or_default();
    }
    // Nothing to do if the transcript has not advanced since the last sync.
    if !candidate.updated_at.is_empty()
        && candidate.updated_at == record.native_updated_at
        && record.jsonl_bytes_synced > 0
    {
        return Ok(());
    }
    let updated_at = candidate.updated_at.clone();
    let source_path = candidate.source_path.clone();

    // OpenCode has no transcript to copy. Its conversations are rows in one
    // store shared by every folder on the machine, and that same file holds the
    // tokens it signs in to its providers with, so the store is never an
    // artifact — what gets mirrored is the document OpenCode hands out when
    // asked for that one session, which is also what it takes back on restore.
    let data = if kind == AgentKind::OpenCode {
        match runtime.export_opencode_session(target, &candidate.id)? {
            Some(document) => document,
            // The session was deleted, or the machine no longer has opencode on
            // it. Either way there is nothing new to mirror and what is already
            // backed up is still the best record of that conversation.
            None => return Ok(()),
        }
    } else if is_local {
        match fs::read(&source_path) {
            Ok(data) => data,
            Err(_) => return Ok(()), // transcript vanished; leave prior backup intact
        }
    } else {
        let temp = std::env::temp_dir().join(format!(
            "muxloom-backup-{}-{}.jsonl",
            sanitize(partition),
            sanitize(&session.id)
        ));
        runtime
            .bridge_pool()
            .download_file(target, source_path.clone(), &temp, |_| {})
            .with_context(|| format!("failed to download transcript {source_path}"))?;
        let data = fs::read(&temp)?;
        let _ = fs::remove_file(&temp);
        data
    };

    store.write_blob(partition, &session.id, TRANSCRIPT_BLOB, &data)?;
    let (messages, title) = extract_messages(kind, &data);
    store.write_blob(
        partition,
        &session.id,
        MESSAGES_BLOB,
        messages_to_jsonl(&messages).as_bytes(),
    )?;
    record.message_count = messages.len();
    record.jsonl_bytes_synced = data.len() as u64;
    record.native_updated_at = updated_at;
    match title {
        // A runtime renames a conversation as it learns what it is about, and
        // the newest name it gave is the truth about it now.
        Some(Title::Named(named)) => record.title = named,
        Some(Title::Guessed(guess)) if record.title.is_empty() => record.title = guess,
        _ => {}
    }
    stats.transcripts += 1;
    Ok(())
}

/// Choose the native transcript for a session.
///
/// A folder holds every conversation ever held in it, and several agents can be
/// running in one folder at once, so this is a matching problem and not a
/// lookup. The daemon solves it properly - see
/// [`crate::native_history::assign_threads`] - so its answer comes first here,
/// and it is allowed to overrule an id this record settled on earlier: that is
/// how a session that was mirroring its neighbour's conversation gets put back
/// on its own.
///
/// The two fallbacks are for what the daemon could not answer: a companion too
/// old to report a thread, or a session it left unmatched. They stay away from
/// transcripts another session on this machine has already been paired with,
/// because picking "the most recently active file in this folder" is exactly
/// how two sessions in one repository ended up sharing one history.
fn pick_native_candidate<'a>(
    candidates: &'a [crate::model::ResumeCandidate],
    session: &crate::daemon_protocol::DaemonSession,
    record: &BackupRecord,
    spoken_for: &HashMap<String, String>,
) -> Option<&'a crate::model::ResumeCandidate> {
    let mine = |id: &str| spoken_for.get(id).is_none_or(|owner| *owner == session.id);
    if let Some(thread) = session.thread.as_deref()
        && let Some(matched) = candidates.iter().find(|c| c.id == thread)
    {
        return Some(matched);
    }
    if !record.native_id.is_empty()
        && mine(&record.native_id)
        && let Some(existing) = candidates.iter().find(|c| c.id == record.native_id)
    {
        return Some(existing);
    }
    // Last resort, and the one that has to be kept honest. A folder holds
    // every conversation ever held in it, so "whichever file was written to
    // most recently" is not an answer for a session nobody could name a thread
    // for: a freshly started agent that has yet to write a word takes whatever
    // the last person to work in that directory left behind. The wrong id then
    // outlives the guess — it is what gets mirrored, what the archive records,
    // and what a later resume reopens.
    //
    // A conversation whose last word predates the session's launch cannot be
    // one this session is writing, so rule that much out. Same slack the
    // daemon's own matching allows, for a transcript stamped a moment before
    // muxloom recorded the launch. A transcript that never said when it was
    // touched stays out of the guess entirely: no id at all is recoverable,
    // and somebody else's is not.
    let started = crate::native_history::iso_timestamp(
        session
            .created_at
            .saturating_mul(1_000)
            .saturating_sub(crate::native_history::START_GRACE_MS),
    );
    candidates
        .iter()
        .filter(|candidate| mine(&candidate.id))
        .filter(|candidate| !candidate.updated_at.is_empty() && candidate.updated_at >= started)
        .max_by(|a, b| a.updated_at.cmp(&b.updated_at))
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

fn messages_to_jsonl(messages: &[ExtractedMessage]) -> String {
    let mut out = String::new();
    for message in messages {
        if let Ok(line) = serde_json::to_string(message) {
            out.push_str(&line);
            out.push('\n');
        }
    }
    out
}

/// Read back what [`messages_to_jsonl`] wrote. A line that no longer parses is
/// skipped, the same way the transcript it came from is read.
fn messages_from_jsonl(raw: &[u8]) -> Vec<ExtractedMessage> {
    String::from_utf8_lossy(raw)
        .lines()
        .filter_map(|line| serde_json::from_str::<ExtractedMessage>(line).ok())
        .collect()
}

/// What a backed-up conversation is called, and how much that is worth.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Title {
    /// The name the runtime gave it. Rewritten as the conversation goes on,
    /// so a newer one always replaces what is on the record.
    Named(String),
    /// The opening of the first thing anyone actually said, for a runtime that
    /// never named it. A guess, and it only fills an empty slot.
    Guessed(String),
}

impl Title {
    pub fn text(&self) -> &str {
        match self {
            Title::Named(text) | Title::Guessed(text) => text,
        }
    }
}

/// Parse an agent-native transcript into a flat message list plus an optional
/// title. Best-effort: unknown line shapes are skipped, consecutive exact
/// duplicates (Codex emits both an `event_msg` and a `response_item` for the
/// same user turn) are collapsed.
pub fn extract_messages(kind: AgentKind, jsonl: &[u8]) -> (Vec<ExtractedMessage>, Option<Title>) {
    // OpenCode's mirror is one document rather than a line per event, so it is
    // read whole instead of walked line by line.
    if kind == AgentKind::OpenCode {
        return extract_opencode_messages(jsonl);
    }
    let text = String::from_utf8_lossy(jsonl);
    let mut messages: Vec<ExtractedMessage> = Vec::new();
    let mut title: Option<Title> = None;
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        match kind {
            AgentKind::Codex => match value.get("type").and_then(Value::as_str) {
                Some("event_msg") => {
                    if let Some(payload) = value.get("payload") {
                        let ts = string_field(&value, "timestamp");
                        match payload.get("type").and_then(Value::as_str) {
                            Some("user_message") => push_message(
                                &mut messages,
                                "user",
                                string_field(payload, "message"),
                                ts,
                            ),
                            Some("agent_message") => push_message(
                                &mut messages,
                                "assistant",
                                string_field(payload, "message"),
                                ts,
                            ),
                            _ => {}
                        }
                    }
                }
                Some("response_item") => {
                    if let Some(payload) = value.get("payload") {
                        let role = payload
                            .get("role")
                            .and_then(Value::as_str)
                            .unwrap_or_default();
                        if role == "user" || role == "assistant" {
                            let body = content_to_text(payload.get("content"));
                            push_message(
                                &mut messages,
                                role,
                                body,
                                string_field(&value, "timestamp"),
                            );
                        }
                    }
                }
                _ => {}
            },
            AgentKind::Claude => {
                // Claude Code renames a session as it learns what it is about,
                // so the last name in the file wins; an older transcript names
                // it once, and that only stands if there is no newer form.
                if let Some(named) = crate::native_history::claude_ai_title(&value) {
                    title = Some(Title::Named(named.to_string()));
                } else if title.is_none()
                    && let Some(explicit) = crate::native_history::claude_legacy_title(&value)
                {
                    title = Some(Title::Named(explicit.to_string()));
                }
                if let Some(role) = value.get("type").and_then(Value::as_str)
                    && (role == "user" || role == "assistant")
                {
                    let content = value
                        .get("message")
                        .and_then(|message| message.get("content"));
                    let body = content_to_text(content);
                    push_message(&mut messages, role, body, string_field(&value, "timestamp"));
                }
            }
            AgentKind::Pi => {
                // pi lets a conversation be named and renamed on a line of its
                // own, so the last name in the file is the one it goes by.
                if let Some(named) = crate::native_history::pi_session_name(&value) {
                    title = Some(Title::Named(named.to_string()));
                }
                if value.get("type").and_then(Value::as_str) == Some("message")
                    && let Some(message) = value.get("message")
                    && let Some(role) = message.get("role").and_then(Value::as_str)
                    // A tool's answer is a message here too, under a role of
                    // its own. It is not something either party said.
                    && (role == "user" || role == "assistant")
                {
                    let body = content_to_text(message.get("content"));
                    push_message(&mut messages, role, body, string_field(&value, "timestamp"));
                }
            }
            // OpenCode was read whole above, and a terminal is a screen rather
            // than a conversation - what was said in one is only ever the
            // capture.
            AgentKind::OpenCode | AgentKind::Terminal => {}
        }
    }
    if title.is_none() {
        title = guess_title(&messages).map(Title::Guessed);
    }
    (messages, title)
}

/// The same flat message list, read out of the document OpenCode exports.
///
/// The shape is one `info` block for the session and a `messages` array, each
/// entry an `info` of its own plus the `parts` it was assembled from. Only the
/// text parts are speech: the rest are the tool calls, the reasoning and the
/// step markers the runtime keeps to rebuild its own view, and none of them is
/// something either party said.
fn extract_opencode_messages(document: &[u8]) -> (Vec<ExtractedMessage>, Option<Title>) {
    let mut messages: Vec<ExtractedMessage> = Vec::new();
    let Ok(value) = serde_json::from_slice::<Value>(document) else {
        return (messages, None);
    };
    // OpenCode names a session itself and renames it as it learns what it is
    // about, so the name in the document is the name it goes by - unless it is
    // still the placeholder it was filed under before it had one.
    let title = value
        .pointer("/info")
        .and_then(crate::native_history::opencode_title)
        .map(|named| Title::Named(named.to_string()));
    for entry in value
        .pointer("/messages")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default()
    {
        let info = entry.pointer("/info");
        let role = info
            .and_then(|info| info.get("role"))
            .and_then(Value::as_str)
            .unwrap_or_default();
        if role != "user" && role != "assistant" {
            continue;
        }
        let ts = info
            .and_then(|info| info.pointer("/time/created"))
            .and_then(Value::as_u64)
            .map(crate::native_history::iso_timestamp)
            .unwrap_or_default();
        let mut spoken = String::new();
        for part in entry
            .pointer("/parts")
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or_default()
        {
            if part.get("type").and_then(Value::as_str) != Some("text") {
                continue;
            }
            let Some(text) = part.get("text").and_then(Value::as_str) else {
                continue;
            };
            if !spoken.is_empty() {
                spoken.push('\n');
            }
            spoken.push_str(text);
        }
        push_message(&mut messages, role, spoken, ts);
    }
    let title = title.or_else(|| guess_title(&messages).map(Title::Guessed));
    (messages, title)
}

/// What to call a conversation whose runtime never named it: the opening of
/// the first thing a person said in it. Everything a runtime files under the
/// person's own role is skipped - see
/// [`crate::native_history::is_spoken`] - or a folder of conversations all
/// end up called the same thing.
fn guess_title(messages: &[ExtractedMessage]) -> Option<String> {
    messages
        .iter()
        .filter(|message| message.role == "user")
        // Judged whole, before it is cut down: a bracketed note that runs past
        // the cut would lose the bracket that gives it away.
        .find(|message| crate::native_history::is_spoken(&message.text))
        .map(|message| truncate_title(&message.text))
}

/// Append a message, dropping empties and collapsing a duplicate of the
/// immediately preceding turn (Codex logs some turns twice).
fn push_message(messages: &mut Vec<ExtractedMessage>, role: &str, body: String, ts: String) {
    let body = body.trim().to_string();
    if body.is_empty() {
        return;
    }
    if let Some(last) = messages.last()
        && last.role == role
        && last.text == body
    {
        return;
    }
    messages.push(ExtractedMessage {
        role: role.to_string(),
        text: body,
        ts,
    });
}

fn string_field(value: &Value, key: &str) -> String {
    value
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

/// Flatten a message `content` field (string, or an array of blocks) to text.
/// Handles both Claude content blocks (`{type:"text", text:..}`) and Codex
/// content parts (`{type:"input_text"/"output_text", text:..}`), skipping
/// tool-call / non-text blocks.
fn content_to_text(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(parts)) => {
            let mut out = String::new();
            for part in parts {
                if let Some(text) = part.get("text").and_then(Value::as_str) {
                    if !out.is_empty() {
                        out.push('\n');
                    }
                    out.push_str(text);
                }
            }
            out
        }
        _ => String::new(),
    }
}

fn truncate_title(text: &str) -> String {
    let first_line = text.lines().next().unwrap_or("").trim();
    first_line.chars().take(80).collect()
}

// ---------------------------------------------------------------------------
// Restore: push a backed-up conversation back onto the machine it came from.
// ---------------------------------------------------------------------------

/// The backed-up sessions of one machine that the machine itself no longer
/// knows about — what a wiped, reimaged or recycled box lost. `alias` is the
/// target id as configured locally; records are partitioned by stable machine
/// key, so it is de-aliased first. Newest first.
///
/// A record is only worth listing if something readable came back with it: a
/// structured transcript (restorable) or a terminal capture (readable). Live
/// sessions are filtered out by the caller's `live_session_ids`, so the result
/// is exactly the history that would otherwise have vanished with the machine.
///
/// Records are collapsed to one per agent-native transcript, newest kept:
/// several muxloom sessions resuming the same conversation share a transcript,
/// and once one of them is live again — which is what a restore leads to — the
/// whole conversation stops being lost and drops off the list.
pub fn recoverable_records(
    store: &BackupStore,
    alias: &str,
    live_session_ids: &HashSet<String>,
) -> Vec<BackupRecord> {
    let index = match store.load_index() {
        Ok(index) => index,
        Err(error) => {
            crate::debug::log("backup", format!("index unreadable: {error:#}"));
            return Vec::new();
        }
    };
    let machine = index.machine_key_for_alias(alias);
    let mine: Vec<BackupRecord> = index
        .records
        .into_iter()
        .filter(|record| record.target_id == machine)
        .filter(|record| record.kind != AgentKind::Terminal.as_str())
        .collect();
    // Conversations that still have a session on the machine are not lost, no
    // matter which of their records the machine happens to still know about.
    let present: HashSet<&str> = mine
        .iter()
        .filter(|record| live_session_ids.contains(&record.session_id))
        .map(|record| record.native_id.as_str())
        .filter(|native_id| !native_id.is_empty())
        .collect();
    let mut records: Vec<BackupRecord> = mine
        .iter()
        .filter(|record| !live_session_ids.contains(&record.session_id))
        .filter(|record| !present.contains(record.native_id.as_str()))
        .filter(|record| {
            record.message_count > 0
                || store.blob_len(&record.target_id, &record.session_id, CAPTURE_BLOB) > 0
        })
        .cloned()
        .collect();
    records.sort_by_key(|record| std::cmp::Reverse(record.created_at));
    let mut seen: HashSet<String> = HashSet::new();
    records.retain(|record| record.native_id.is_empty() || seen.insert(record.native_id.clone()));
    records
}

/// Whether a record carries the structured transcript a restore needs. Records
/// with only a terminal capture are readable but cannot be resumed.
pub fn is_restorable(record: &BackupRecord) -> bool {
    !record.native_id.is_empty() && restore_route(record).is_some() && record.jsonl_bytes_synced > 0
}

/// How a backed-up conversation gets back onto a machine.
///
/// Three of the runtimes read a file, so putting one back is writing that file
/// where the runtime looks for it. OpenCode reads a store it shares with every
/// folder on the machine and keeps its provider tokens in, so nothing muxloom
/// holds may be written over it: the document goes back through OpenCode's own
/// import, which is the only thing entitled to touch that file.
enum RestoreRoute {
    /// The `$HOME`-relative path to write the mirrored bytes to.
    File(String),
    /// Hand the document to OpenCode and let it file the session itself.
    Import,
}

fn restore_route(record: &BackupRecord) -> Option<RestoreRoute> {
    if record.kind == AgentKind::OpenCode.as_str() {
        return Some(RestoreRoute::Import);
    }
    native_relative_path(&record.native_path).map(RestoreRoute::File)
}

/// Restore one session of one machine from the default store, looked up by the
/// key the UI carries. See [`restore_transcript`].
pub fn restore_session(
    runtime: &Runtime,
    target: &Target,
    machine_key: &str,
    session_id: &str,
) -> Result<RestoredTranscript> {
    let store = BackupStore::new(BackupStore::default_root());
    let index = store.load_index()?;
    let record = index
        .position(machine_key, session_id)
        .map(|position| index.records[position].clone())
        .with_context(|| format!("{session_id} is not in the backup of {machine_key}"))?;
    restore_transcript(runtime, &store, target, &record)
}

/// Put a backed-up conversation back onto `target` where the runtime's own
/// resume will find it — written to the agent-native location, or handed to
/// OpenCode to file itself, per [`RestoreRoute`] — and return the id to resume
/// with. The local blob is left untouched — this copies out, it does not move.
pub fn restore_transcript(
    runtime: &Runtime,
    store: &BackupStore,
    target: &Target,
    record: &BackupRecord,
) -> Result<RestoredTranscript> {
    if record.native_id.is_empty() {
        bail!("only this session's terminal output was backed up, not a resumable transcript");
    }
    let route = restore_route(record).with_context(|| {
        format!(
            "cannot place a transcript from an unrecognised path: {}",
            record.native_path
        )
    })?;
    if let RestoreRoute::Import = route
        && record.cwd.is_empty()
    {
        bail!(
            "the backup does not say which folder this opencode session belongs to, and one \
             filed under the wrong folder is one nobody working there is offered"
        );
    }
    let data = store.read_blob(&record.target_id, &record.session_id, TRANSCRIPT_BLOB)?;
    if data.is_empty() {
        bail!("the backed-up transcript is empty");
    }
    // The path is rebuilt from the target's own HOME rather than replayed
    // verbatim: the same machine can come back with a different home, and a
    // record may be restored onto a box that is not the one it came from.
    let placement = match &route {
        RestoreRoute::File(relative) => Some(runtime.home_relative_path(target, relative)?),
        RestoreRoute::Import => None,
    };
    let temp = std::env::temp_dir().join(format!(
        "muxloom-restore-{}-{}.{}",
        sanitize(&record.target_id),
        sanitize(&record.session_id),
        match route {
            RestoreRoute::File(_) => "jsonl",
            RestoreRoute::Import => "json",
        }
    ));
    fs::write(&temp, &data)
        .with_context(|| format!("failed to stage restore in {}", temp.display()))?;
    let destination = match placement {
        Some(destination) => {
            let placed = runtime.place_file(target, &temp, &destination);
            let _ = fs::remove_file(&temp);
            placed.with_context(|| format!("failed to restore transcript to {destination}"))?;
            destination
        }
        None => {
            let imported =
                runtime.import_opencode_session(target, &record.cwd, &record.native_id, &temp);
            let _ = fs::remove_file(&temp);
            imported.with_context(|| {
                format!(
                    "failed to hand the session back to opencode in {} on {}",
                    record.cwd, target.id
                )
            })?
        }
    };
    crate::debug::log(
        "backup",
        format!(
            "restored {} bytes of {} to {}:{destination}",
            data.len(),
            record.session_id,
            target.id
        ),
    );
    Ok(RestoredTranscript {
        resume_id: record.native_id.clone(),
        path: destination,
        bytes: data.len() as u64,
    })
}

/// The `$HOME`-relative tail of an agent-native transcript path
/// (`.claude/projects/…`, `.codex/sessions/…` or `.pi/agent/sessions/…`), or
/// None if the path is not one an agent would look in.
fn native_relative_path(native_path: &str) -> Option<String> {
    [".claude/", ".codex/", ".pi/"]
        .into_iter()
        .find_map(|marker| {
            native_path
                .find(marker)
                .map(|position| native_path[position..].to_string())
        })
}

// ---------------------------------------------------------------------------
// Search: query the aggregated backup across all machines.
// ---------------------------------------------------------------------------

/// One matching message in the backup, with enough context to display and open
/// the conversation it belongs to (possibly on another machine).
#[derive(Debug, Clone)]
pub struct SearchHit {
    pub target_id: String,
    pub session_id: String,
    pub kind: String,
    pub cwd: String,
    pub created_at: u64,
    pub title: String,
    /// Role of the matching message (`user` / `assistant`), or `title`.
    pub role: String,
    /// A one-line excerpt around the match.
    pub snippet: String,
    /// Match count in the message — used for ranking.
    pub score: usize,
    /// Which message in the conversation matched, counted from the start —
    /// the line number in the extracted jsonl, and what a reader pages around.
    /// `usize::MAX` when the match was in the title rather than a message.
    pub message_index: usize,
    /// The agent's own timestamp for the matching message, verbatim and
    /// possibly empty: transcripts write it in their own format.
    pub ts: String,
}

/// Which conversations a search will look at. Every empty field means "all of
/// them", so the default filter searches everything.
#[derive(Debug, Default, Clone)]
pub struct SearchFilter {
    /// Machine partition keys.
    pub machines: Vec<String>,
    /// Working directories: a conversation counts if it was held in one of
    /// them or anywhere below it.
    pub paths: Vec<String>,
    /// Agent kinds (`codex`, `claude`, `terminal`).
    pub kinds: Vec<String>,
    /// Bounds on when the conversation started, epoch ms. 0 is unbounded.
    pub since: u64,
    pub until: u64,
}

impl SearchFilter {
    pub fn keeps(&self, record: &BackupRecord) -> bool {
        if !self.machines.is_empty() && !self.machines.contains(&record.target_id) {
            return false;
        }
        if !self.kinds.is_empty() && !self.kinds.contains(&record.kind) {
            return false;
        }
        if !self.paths.is_empty() && !self.paths.iter().any(|path| within(&record.cwd, path)) {
            return false;
        }
        if self.since > 0 && record.created_at < self.since {
            return false;
        }
        if self.until > 0 && record.created_at > self.until {
            return false;
        }
        true
    }
}

/// Whether `cwd` is `root` or sits below it.
fn within(cwd: &str, root: &str) -> bool {
    let root = root.trim_end_matches('/');
    cwd == root
        || cwd
            .strip_prefix(root)
            .is_some_and(|rest| rest.starts_with('/'))
}

/// Full-text search across every backed-up conversation (all machines, running
/// and archived). Case-insensitive substring match over extracted messages and
/// titles; results are ranked by match count then recency and capped at `limit`.
pub fn search(store: &BackupStore, query: &str, limit: usize) -> Result<Vec<SearchHit>> {
    search_where(store, query, limit, &SearchFilter::default())
}

/// The same search, over the conversations `filter` admits.
pub fn search_where(
    store: &BackupStore,
    query: &str,
    limit: usize,
    filter: &SearchFilter,
) -> Result<Vec<SearchHit>> {
    let needle = query.trim().to_lowercase();
    if needle.is_empty() {
        return Ok(Vec::new());
    }
    let index = store.load_index()?;
    let mut hits: Vec<SearchHit> = Vec::new();
    for record in index.records.iter().filter(|record| filter.keeps(record)) {
        let mut matched_message = false;
        let raw = store
            .read_blob(&record.target_id, &record.session_id, MESSAGES_BLOB)
            .unwrap_or_default();
        // Enumerated over every line, parseable or not, so an index here is the
        // same index `read_messages` pages around.
        for (position, line) in String::from_utf8_lossy(&raw).lines().enumerate() {
            let Ok(message) = serde_json::from_str::<ExtractedMessage>(line) else {
                continue;
            };
            let count = message.text.to_lowercase().matches(&needle).count();
            if count == 0 {
                continue;
            }
            matched_message = true;
            hits.push(SearchHit {
                target_id: record.target_id.clone(),
                session_id: record.session_id.clone(),
                kind: record.kind.clone(),
                cwd: record.cwd.clone(),
                created_at: record.created_at,
                title: record.title.clone(),
                role: message.role,
                snippet: make_snippet(&message.text, &needle),
                score: count,
                message_index: position,
                ts: message.ts,
            });
        }
        // Surface a title/recap match even when no message body matched.
        if !matched_message {
            let haystack = format!("{} {}", record.title, record.recap).to_lowercase();
            if haystack.contains(&needle) {
                hits.push(SearchHit {
                    target_id: record.target_id.clone(),
                    session_id: record.session_id.clone(),
                    kind: record.kind.clone(),
                    cwd: record.cwd.clone(),
                    created_at: record.created_at,
                    title: record.title.clone(),
                    role: "title".into(),
                    snippet: make_snippet(
                        if record.title.is_empty() {
                            &record.recap
                        } else {
                            &record.title
                        },
                        &needle,
                    ),
                    score: 1,
                    message_index: usize::MAX,
                    ts: String::new(),
                });
            }
        }
    }
    hits.sort_by(|a, b| b.score.cmp(&a.score).then(b.created_at.cmp(&a.created_at)));
    hits.truncate(limit);
    Ok(hits)
}

/// A window of one backed-up conversation, addressed by message index: the
/// line number in the extracted jsonl, which is what a search hit reports.
///
/// Returns the messages in the window, each with its index, and how many the
/// conversation holds in total — enough for a caller to know whether there is
/// anything before or after what it asked for.
pub fn read_messages(
    store: &BackupStore,
    target_id: &str,
    session_id: &str,
    from: usize,
    limit: usize,
) -> Result<(Vec<(usize, ExtractedMessage)>, usize)> {
    let raw = store.read_blob(target_id, session_id, MESSAGES_BLOB)?;
    let text = String::from_utf8_lossy(&raw);
    let mut window = Vec::new();
    let mut total = 0;
    for (position, line) in text.lines().enumerate() {
        total = position + 1;
        if position < from || window.len() >= limit {
            continue;
        }
        if let Ok(message) = serde_json::from_str::<ExtractedMessage>(line) {
            window.push((position, message));
        }
    }
    Ok((window, total))
}

/// A one-line excerpt of `text` centered on the first match of `needle`
/// (already lowercased). Char-based so multibyte text is never split.
fn make_snippet(text: &str, needle: &str) -> String {
    let lowered = text.to_lowercase();
    let Some(byte_pos) = lowered.find(needle) else {
        let head: String = text.chars().take(100).collect();
        return head.replace('\n', " ").trim().to_string();
    };
    let char_pos = lowered[..byte_pos].chars().count();
    let chars: Vec<char> = text.chars().collect();
    let start = char_pos.saturating_sub(36);
    let end = (char_pos + needle.chars().count() + 60).min(chars.len());
    let mut snippet = String::new();
    if start > 0 {
        snippet.push('…');
    }
    snippet.extend(chars[start.min(chars.len())..end].iter());
    if end < chars.len() {
        snippet.push('…');
    }
    snippet.replace('\n', " ").trim().to_string()
}

/// Sanitize a path component so a target/session id can name a directory.
fn sanitize(component: &str) -> String {
    let cleaned: String = component
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.') {
                character
            } else {
                '_'
            }
        })
        .collect();
    if cleaned.is_empty() {
        "_".into()
    } else {
        cleaned
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A store rooted at a unique temp dir. The returned guard removes the tree
    /// on drop, mirroring the `env::temp_dir()` pattern used elsewhere.
    struct TempStore {
        store: BackupStore,
        root: PathBuf,
    }

    impl std::ops::Deref for TempStore {
        type Target = BackupStore;
        fn deref(&self) -> &BackupStore {
            &self.store
        }
    }

    impl Drop for TempStore {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    fn temp_store() -> TempStore {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("muxloom-backup-{nonce}"));
        TempStore {
            store: BackupStore::new(root.clone()),
            root,
        }
    }

    #[test]
    fn appended_frames_stitch_back_into_one_stream() {
        let store = temp_store();
        store
            .append_frame("local", "s1", CAPTURE_BLOB, b"hello ")
            .unwrap();
        store
            .append_frame("local", "s1", CAPTURE_BLOB, b"world")
            .unwrap();
        // A third empty append is a no-op and must not corrupt the stream.
        store
            .append_frame("local", "s1", CAPTURE_BLOB, b"")
            .unwrap();
        let out = store.read_blob("local", "s1", CAPTURE_BLOB).unwrap();
        assert_eq!(out, b"hello world");
    }

    #[test]
    fn reading_the_tail_stops_at_the_newest_output_it_was_asked_for() {
        let store = temp_store();
        for index in 0..40u32 {
            store
                .append_frame(
                    "local",
                    "s1",
                    CAPTURE_BLOB,
                    format!("line {index:02}\n").as_bytes(),
                )
                .unwrap();
        }
        let whole = store.read_blob("local", "s1", CAPTURE_BLOB).unwrap();
        assert_eq!(whole.len(), 40 * "line 00\n".len());

        // A budget smaller than the blob returns its end, cut to whole lines,
        // and says so.
        let (tail, clipped) = store
            .read_blob_tail("local", "s1", CAPTURE_BLOB, 30)
            .unwrap();
        assert!(clipped);
        assert!(
            tail.len() <= 30,
            "read {} bytes for a 30 budget",
            tail.len()
        );
        let text = String::from_utf8(tail).unwrap();
        assert!(text.starts_with("line "), "cut mid-line: {text:?}");
        assert!(text.ends_with("line 39\n"));

        // A budget past the end returns everything, unclipped.
        let (all, clipped) = store
            .read_blob_tail("local", "s1", CAPTURE_BLOB, 10_000)
            .unwrap();
        assert!(!clipped);
        assert_eq!(all, whole);

        // One huge frame is cut too, not returned whole because it is one piece.
        store
            .write_blob("local", "s2", CAPTURE_BLOB, &vec![b'x'; 5_000])
            .unwrap();
        let (tail, clipped) = store
            .read_blob_tail("local", "s2", CAPTURE_BLOB, 100)
            .unwrap();
        assert!(clipped);
        assert_eq!(tail.len(), 100);

        // A missing blob is empty, not an error.
        assert_eq!(
            store
                .read_blob_tail("local", "gone", CAPTURE_BLOB, 100)
                .unwrap(),
            (Vec::new(), false)
        );
    }

    #[test]
    fn read_blob_is_empty_when_absent() {
        let store = temp_store();
        assert!(
            store
                .read_blob("local", "missing", CAPTURE_BLOB)
                .unwrap()
                .is_empty()
        );
        assert_eq!(store.blob_len("local", "missing", CAPTURE_BLOB), 0);
    }

    #[test]
    fn write_blob_overwrites_with_single_frame() {
        let store = temp_store();
        store
            .write_blob("local", "s1", TRANSCRIPT_BLOB, b"first")
            .unwrap();
        store
            .write_blob("local", "s1", TRANSCRIPT_BLOB, b"second value")
            .unwrap();
        let out = store.read_blob("local", "s1", TRANSCRIPT_BLOB).unwrap();
        assert_eq!(out, b"second value");
    }

    #[test]
    fn index_roundtrips_and_upserts_by_key() {
        let store = temp_store();
        let mut index = store.load_index().unwrap();
        assert!(index.records.is_empty());
        index.upsert(BackupRecord {
            target_id: "local".into(),
            session_id: "s1".into(),
            kind: "codex".into(),
            message_count: 3,
            ..Default::default()
        });
        index.upsert(BackupRecord {
            target_id: "local".into(),
            session_id: "s1".into(),
            kind: "codex".into(),
            message_count: 7, // update, not duplicate
            ..Default::default()
        });
        store.save_index(&index).unwrap();
        let reloaded = store.load_index().unwrap();
        assert_eq!(reloaded.records.len(), 1);
        assert_eq!(reloaded.records[0].message_count, 7);
    }

    #[test]
    fn old_index_json_loads_with_defaults_for_new_fields() {
        let store = temp_store();
        // Simulate an index written before newer fields existed.
        fs::create_dir_all(store.root()).unwrap();
        fs::write(
            store.index_path(),
            r#"{"records":[{"target_id":"h20","session_id":"muxloomd-codex-1-2-3","kind":"codex"}]}"#,
        )
        .unwrap();
        let index = store.load_index().unwrap();
        assert_eq!(index.records.len(), 1);
        assert!(index.machines.is_empty()); // new field defaults to empty
        let record = &index.records[0];
        assert_eq!(record.target_id, "h20");
        assert_eq!(record.ansi_lines_synced, 0); // defaulted
        assert!(!record.dead); // defaulted
    }

    #[test]
    fn extracts_codex_messages_and_collapses_duplicates() {
        let jsonl = concat!(
            r#"{"type":"session_meta","payload":{"id":"019f-abc","cwd":"/work","timestamp":"2026-07-24T19:42:01"}}"#,
            "\n",
            r#"{"type":"event_msg","timestamp":"t1","payload":{"type":"user_message","message":"how do I scroll?"}}"#,
            "\n",
            // response_item duplicate of the same user turn must collapse
            r#"{"type":"response_item","timestamp":"t1","payload":{"role":"user","content":[{"type":"input_text","text":"how do I scroll?"}]}}"#,
            "\n",
            r#"{"type":"response_item","timestamp":"t2","payload":{"role":"assistant","content":[{"type":"output_text","text":"press Ctrl+T"}]}}"#,
            "\n",
        );
        let (messages, title) = extract_messages(AgentKind::Codex, jsonl.as_bytes());
        assert_eq!(messages.len(), 2, "duplicate user turn should collapse");
        assert_eq!(messages[0].role, "user");
        assert_eq!(messages[0].text, "how do I scroll?");
        assert_eq!(messages[1].role, "assistant");
        assert_eq!(messages[1].text, "press Ctrl+T");
        assert_eq!(title, Some(Title::Guessed("how do I scroll?".into())));
    }

    /// What a runtime files under the person's own role is not a name for the
    /// conversation, and a guess at one is only worth keeping until the
    /// runtime says what it is really about.
    #[test]
    fn a_guessed_name_skips_the_machinery_and_gives_way_to_a_real_one() {
        let jsonl = concat!(
            r#"{"type":"user","timestamp":"t1","message":{"content":"<local-command-caveat>Caveat: The messages below were generated by the user while running local commands.</local-command-caveat>"}}"#,
            "\n",
            r#"{"type":"user","timestamp":"t2","message":{"content":"<command-name>/clear</command-name>"}}"#,
            "\n",
            r#"{"type":"user","timestamp":"t3","message":{"content":"why does the daemon lose the title?"}}"#,
            "\n",
        );
        let (_, guessed) = extract_messages(AgentKind::Claude, jsonl.as_bytes());
        assert_eq!(
            guessed,
            Some(Title::Guessed("why does the daemon lose the title?".into()))
        );

        let named = format!(
            "{jsonl}{}\n",
            r#"{"type":"ai-title","aiTitle":"reading the daemon"}"#
        );
        assert_eq!(
            extract_messages(AgentKind::Claude, named.as_bytes()).1,
            Some(Title::Named("reading the daemon".into())),
            "once the runtime names it, the guess is beside the point"
        );

        // Nothing was said that anybody said, so there is nothing to call it.
        let only_machinery = concat!(
            r#"{"type":"user","timestamp":"t1","message":{"content":"[Request interrupted by user]"}}"#,
            "\n",
        );
        assert_eq!(
            extract_messages(AgentKind::Claude, only_machinery.as_bytes()).1,
            None
        );
    }

    #[test]
    fn extracts_claude_messages_string_and_block_content() {
        let jsonl = concat!(
            r#"{"type":"summary","summary":"scrolling help"}"#,
            "\n",
            r#"{"type":"user","timestamp":"t1","sessionId":"claude-1","message":{"content":"hello there"}}"#,
            "\n",
            r#"{"type":"assistant","timestamp":"t2","message":{"content":[{"type":"text","text":"hi"},{"type":"tool_use","name":"bash"},{"type":"text","text":"world"}]}}"#,
            "\n",
        );
        let (messages, title) = extract_messages(AgentKind::Claude, jsonl.as_bytes());
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].text, "hello there");
        // Text blocks concatenated, tool block skipped.
        assert_eq!(messages[1].text, "hi\nworld");
        assert_eq!(title, Some(Title::Named("scrolling help".into())));
    }

    /// The name a current Claude Code writes outranks a summary from an older
    /// one, and the last of those names is the session as it ended up.
    #[test]
    fn the_latest_name_claude_gave_a_session_becomes_its_title() {
        let jsonl = concat!(
            r#"{"type":"summary","summary":"named by an older build"}"#,
            "\n",
            r#"{"type":"ai-title","aiTitle":"reading the daemon","sessionId":"claude-1"}"#,
            "\n",
            r#"{"type":"user","timestamp":"t1","sessionId":"claude-1","message":{"content":"hello there"}}"#,
            "\n",
            r#"{"type":"ai-title","aiTitle":"recap from the transcript","sessionId":"claude-1"}"#,
            "\n",
        );
        let (_, title) = extract_messages(AgentKind::Claude, jsonl.as_bytes());
        assert_eq!(
            title,
            Some(Title::Named("recap from the transcript".into()))
        );
    }

    /// OpenCode's mirror is one document, not a line per event, and only the
    /// text parts of a message are speech - the tool call it made and the
    /// thinking it did on the way are how it got there, not what it said.
    #[test]
    fn extracts_what_was_said_in_an_opencode_session() {
        let document = r#"{
          "info": { "id": "ses_1", "title": "reading the store", "directory": "/work" },
          "messages": [
            { "info": { "role": "user", "time": { "created": 1750000000000 } },
              "parts": [ { "type": "text", "text": "where does it keep them" } ] },
            { "info": { "role": "assistant", "time": { "created": 1750000001000 } },
              "parts": [ { "type": "reasoning", "text": "think" },
                         { "type": "text", "text": "in one store" },
                         { "type": "tool", "tool": "bash" },
                         { "type": "text", "text": "shared by every folder" },
                         { "type": "step-finish" } ] },
            { "info": { "role": "system" }, "parts": [ { "type": "text", "text": "ignored" } ] }
          ]
        }"#;
        let (messages, title) = extract_messages(AgentKind::OpenCode, document.as_bytes());
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].role, "user");
        assert_eq!(messages[0].text, "where does it keep them");
        assert_eq!(messages[0].ts, "2025-06-15T15:06:40.000Z");
        assert_eq!(messages[1].text, "in one store\nshared by every folder");
        assert_eq!(title, Some(Title::Named("reading the store".into())));

        // Nothing OpenCode could hand over is worth a panic: a document that is
        // not one comes back empty rather than taking the sync down with it.
        let (nothing, unnamed) = extract_messages(AgentKind::OpenCode, b"not a document");
        assert!(nothing.is_empty());
        assert_eq!(unnamed, None);
    }

    /// A session OpenCode has not named yet is filed under a placeholder, and
    /// letting that stand would call a folder of conversations the same thing.
    #[test]
    fn an_unnamed_opencode_session_is_titled_from_what_was_said() {
        let document = r#"{
          "info": { "id": "ses_2", "title": "New session - 2026-08-25" },
          "messages": [
            { "info": { "role": "user" },
              "parts": [ { "type": "text", "text": "trace the resume path" } ] }
          ]
        }"#;
        let (_, title) = extract_messages(AgentKind::OpenCode, document.as_bytes());
        assert_eq!(title, Some(Title::Guessed("trace the resume path".into())));
    }

    /// An OpenCode record has no transcript path to put back, because there is
    /// no file - it goes home through OpenCode's own import, and the folder it
    /// belonged to is part of the address.
    #[test]
    fn an_opencode_session_is_restored_by_handing_it_back() {
        let mut record = BackupRecord {
            kind: AgentKind::OpenCode.as_str().into(),
            native_id: "ses_1".into(),
            native_path: String::new(),
            cwd: "/work".into(),
            jsonl_bytes_synced: 40,
            ..Default::default()
        };
        assert!(matches!(restore_route(&record), Some(RestoreRoute::Import)));
        assert!(is_restorable(&record));

        // The same record from a runtime that does write a file is not
        // restorable without one, which is what it was before.
        record.kind = AgentKind::Claude.as_str().into();
        assert!(restore_route(&record).is_none());
        assert!(!is_restorable(&record));
        record.native_path = "/home/someone/.claude/projects/work/ses.jsonl".into();
        assert!(matches!(
            restore_route(&record),
            Some(RestoreRoute::File(relative)) if relative == ".claude/projects/work/ses.jsonl"
        ));
    }

    #[test]
    fn messages_jsonl_roundtrips_through_a_blob() {
        let store = temp_store();
        let (messages, _) = extract_messages(
            AgentKind::Claude,
            br#"{"type":"user","message":{"content":"q"}}"#,
        );
        let jsonl = messages_to_jsonl(&messages);
        store
            .write_blob("local", "s1", MESSAGES_BLOB, jsonl.as_bytes())
            .unwrap();
        let back = store.read_blob("local", "s1", MESSAGES_BLOB).unwrap();
        assert_eq!(back, jsonl.as_bytes());
        assert!(String::from_utf8_lossy(&back).contains("\"q\""));
    }

    #[test]
    fn search_ranks_matches_across_sessions_and_machines() {
        let store = temp_store();
        // Two sessions on different machines, with overlapping content.
        let write = |target: &str, session: &str, msgs: &[(&str, &str)], created: u64| {
            let messages: Vec<ExtractedMessage> = msgs
                .iter()
                .map(|(role, text)| ExtractedMessage {
                    role: role.to_string(),
                    text: text.to_string(),
                    ts: String::new(),
                })
                .collect();
            store
                .write_blob(
                    target,
                    session,
                    MESSAGES_BLOB,
                    messages_to_jsonl(&messages).as_bytes(),
                )
                .unwrap();
            let mut index = store.load_index().unwrap();
            index.upsert(BackupRecord {
                target_id: target.into(),
                session_id: session.into(),
                kind: "claude".into(),
                created_at: created,
                message_count: messages.len(),
                ..Default::default()
            });
            store.save_index(&index).unwrap();
        };
        write("local", "s1", &[("user", "how do I scroll the pager")], 100);
        write(
            "h20",
            "s2",
            &[
                ("user", "scroll scroll scroll again"),
                ("assistant", "press PageDown"),
            ],
            200,
        );

        let hits = search(&store, "scroll", 10).unwrap();
        // s2's user message has 3 occurrences → ranks first (higher score).
        assert!(hits.len() >= 2);
        assert_eq!(hits[0].target_id, "h20");
        assert_eq!(hits[0].score, 3);
        assert!(hits[0].snippet.to_lowercase().contains("scroll"));
        // Cross-machine: a local hit is also present.
        assert!(hits.iter().any(|hit| hit.target_id == "local"));
        // Empty query yields nothing.
        assert!(search(&store, "   ", 10).unwrap().is_empty());
    }

    #[test]
    fn a_narrowed_search_says_which_message_matched_and_reads_back_around_it() {
        let store = temp_store();
        let write = |target: &str,
                     session: &str,
                     cwd: &str,
                     kind: &str,
                     created: u64,
                     msgs: &[(&str, &str)]| {
            let messages: Vec<ExtractedMessage> = msgs
                .iter()
                .map(|(role, text)| ExtractedMessage {
                    role: (*role).into(),
                    text: (*text).into(),
                    ts: String::new(),
                })
                .collect();
            store
                .write_blob(
                    target,
                    session,
                    MESSAGES_BLOB,
                    messages_to_jsonl(&messages).as_bytes(),
                )
                .unwrap();
            let mut index = store.load_index().unwrap();
            index.upsert(BackupRecord {
                target_id: target.into(),
                session_id: session.into(),
                kind: kind.into(),
                cwd: cwd.into(),
                created_at: created,
                message_count: messages.len(),
                ..Default::default()
            });
            store.save_index(&index).unwrap();
        };
        write(
            "local",
            "s1",
            "/work/loom",
            "claude",
            100,
            &[
                ("user", "where is the parser"),
                ("assistant", "in src/parse.rs"),
                ("user", "the parser again"),
            ],
        );
        write(
            "gpu",
            "s2",
            "/work/other",
            "codex",
            300,
            &[("user", "the parser lives elsewhere")],
        );

        let narrow = |filter: SearchFilter| search_where(&store, "parser", 10, &filter).unwrap();
        assert_eq!(search(&store, "parser", 10).unwrap().len(), 3);
        assert_eq!(
            narrow(SearchFilter {
                machines: vec!["gpu".into()],
                ..SearchFilter::default()
            })
            .len(),
            1
        );
        assert_eq!(
            narrow(SearchFilter {
                kinds: vec!["claude".into()],
                ..SearchFilter::default()
            })
            .len(),
            2
        );
        // A directory takes everything below it, and only what is below it: a
        // path is a place, not a prefix.
        assert_eq!(
            narrow(SearchFilter {
                paths: vec!["/work".into()],
                ..SearchFilter::default()
            })
            .len(),
            3
        );
        assert_eq!(
            narrow(SearchFilter {
                paths: vec!["/work/loom".into()],
                ..SearchFilter::default()
            })
            .len(),
            2
        );
        assert!(
            narrow(SearchFilter {
                paths: vec!["/work/loo".into()],
                ..SearchFilter::default()
            })
            .is_empty()
        );
        assert_eq!(
            narrow(SearchFilter {
                since: 200,
                ..SearchFilter::default()
            })
            .len(),
            1
        );
        assert_eq!(
            narrow(SearchFilter {
                until: 200,
                ..SearchFilter::default()
            })
            .len(),
            2
        );

        // A hit says which message it was, and that index reads back as the
        // same message on the same conversation.
        let late = narrow(SearchFilter {
            machines: vec!["local".into()],
            ..SearchFilter::default()
        })
        .into_iter()
        .find(|hit| hit.snippet.contains("again"))
        .expect("the second mention is a hit of its own");
        assert_eq!(late.message_index, 2);
        let (window, total) = read_messages(&store, "local", "s1", 1, 2).unwrap();
        assert_eq!(total, 3);
        assert_eq!(
            window.iter().map(|(index, _)| *index).collect::<Vec<_>>(),
            [1, 2]
        );
        assert_eq!(window[1].1.text, "the parser again");

        // Reading past the end is an empty page that still says how long the
        // conversation is, not an error.
        let (past, total) = read_messages(&store, "local", "s1", 9, 5).unwrap();
        assert!(past.is_empty());
        assert_eq!(total, 3);
    }

    #[test]
    fn large_redundant_capture_compresses_and_roundtrips() {
        let store = temp_store();
        // Mimic a full-screen redraw stream: highly repetitive.
        let chunk = "\x1b[2J\x1b[H".repeat(50_000);
        store
            .append_frame("local", "s1", CAPTURE_BLOB, chunk.as_bytes())
            .unwrap();
        let compressed = store.blob_len("local", "s1", CAPTURE_BLOB);
        assert!(
            (compressed as usize) < chunk.len() / 10,
            "expected >10x compression, got {compressed} vs {}",
            chunk.len()
        );
        let out = store.read_blob("local", "s1", CAPTURE_BLOB).unwrap();
        assert_eq!(out, chunk.as_bytes());
    }

    #[test]
    fn parses_ssh_config_g_endpoint() {
        let text = "user tiger\nhostname 2605:340::2439\nport 9251\nforwardagent no\n";
        let endpoint = parse_ssh_config_g(text);
        assert_eq!(endpoint.host, "2605:340::2439");
        assert_eq!(endpoint.user, "tiger");
        assert_eq!(endpoint.port, "9251");
    }

    #[test]
    fn parses_known_host_fingerprints_dedup() {
        let text = concat!(
            "# Host [h]:9251 found: line 56\n",
            "[h]:9251 ED25519 SHA256:AAA\n",
            "# Host [h]:9251 found: line 57\n",
            "[h]:9251 RSA SHA256:BBB\n",
            "[h]:9251 ECDSA SHA256:AAA\n", // duplicate token
        );
        let fingerprints = parse_known_host_fingerprints(text);
        assert_eq!(
            fingerprints,
            vec!["SHA256:AAA".to_string(), "SHA256:BBB".to_string()]
        );
    }

    fn ssh_probe(
        alias: &str,
        fingerprints: &[&str],
        endpoint: Option<MachineEndpoint>,
    ) -> IdentityProbe {
        IdentityProbe {
            alias: alias.to_string(),
            fingerprints: fingerprints.iter().map(|f| f.to_string()).collect(),
            endpoint,
        }
    }

    #[test]
    fn resolve_machine_registers_then_reuses_by_alias() {
        let mut machines = Vec::new();
        let key = resolve_machine(&mut machines, &ssh_probe("h20", &["SHA256:X"], None), 100);
        assert_eq!(key, "h20");
        assert_eq!(machines.len(), 1);
        assert_eq!(machines[0].aliases, vec!["h20".to_string()]);
        // Same alias again → same machine, no duplicate.
        let key2 = resolve_machine(&mut machines, &ssh_probe("h20", &[], None), 200);
        assert_eq!(key2, "h20");
        assert_eq!(machines.len(), 1);
        assert_eq!(machines[0].last_seen, 200);
        assert_eq!(machines[0].first_seen, 100);
    }

    #[test]
    fn resolve_machine_merges_new_alias_by_fingerprint() {
        let mut machines = Vec::new();
        resolve_machine(&mut machines, &ssh_probe("h20", &["SHA256:X"], None), 100);
        // A different alias but overlapping host-key fingerprint → same machine.
        let key = resolve_machine(
            &mut machines,
            &ssh_probe("h20-alt", &["SHA256:X"], None),
            150,
        );
        assert_eq!(key, "h20", "fingerprint overlap keeps the first-seen key");
        assert_eq!(machines.len(), 1);
        assert_eq!(
            machines[0].aliases,
            vec!["h20".to_string(), "h20-alt".to_string()]
        );
    }

    #[test]
    fn resolve_machine_merges_by_endpoint_when_no_fingerprint() {
        let endpoint = MachineEndpoint {
            host: "h".into(),
            user: "u".into(),
            port: "9".into(),
        };
        let mut machines = Vec::new();
        resolve_machine(
            &mut machines,
            &ssh_probe("a", &[], Some(endpoint.clone())),
            100,
        );
        // No alias/fingerprint overlap, but same endpoint → same machine.
        let key = resolve_machine(&mut machines, &ssh_probe("b", &[], Some(endpoint)), 150);
        assert_eq!(key, "a");
        assert_eq!(machines.len(), 1);
    }

    #[test]
    fn resolve_machine_alias_beats_fingerprint() {
        // Machine A owns alias "a"; machine B owns fingerprint "SHA256:X".
        let mut machines = vec![
            MachineIdentity {
                key: "a".into(),
                aliases: vec!["a".into()],
                ..Default::default()
            },
            MachineIdentity {
                key: "b".into(),
                aliases: vec!["b".into()],
                fingerprints: vec!["SHA256:X".into()],
                ..Default::default()
            },
        ];
        // A probe matching A by alias AND B by fingerprint must resolve to A.
        let key = resolve_machine(&mut machines, &ssh_probe("a", &["SHA256:X"], None), 200);
        assert_eq!(key, "a");
        assert_eq!(machines.len(), 2, "no new machine created");
    }

    #[test]
    fn resolve_machine_distinct_boxes_stay_separate() {
        let mut machines = Vec::new();
        let k1 = resolve_machine(&mut machines, &ssh_probe("a", &["SHA256:X"], None), 100);
        let k2 = resolve_machine(&mut machines, &ssh_probe("b", &["SHA256:Y"], None), 100);
        assert_ne!(k1, k2);
        assert_eq!(machines.len(), 2);
    }

    #[test]
    fn index_maps_alias_to_machine_key() {
        let mut index = BackupIndex::default();
        index.machines.push(MachineIdentity {
            key: "h20".into(),
            aliases: vec!["h20".into(), "h20-alt".into()],
            ..Default::default()
        });
        assert_eq!(index.machine_key_for_alias("h20-alt"), "h20");
        // Unknown alias falls back to itself.
        assert_eq!(index.machine_key_for_alias("other"), "other");
    }

    fn live_session(id: &str, thread: Option<&str>) -> crate::daemon_protocol::DaemonSession {
        crate::daemon_protocol::DaemonSession {
            id: id.into(),
            kind: "claude".into(),
            path: "/home/me/work".into(),
            label: id.into(),
            temporary: false,
            created_at: 0,
            pid: None,
            dead: false,
            archived: false,
            recap: None,
            title: None,
            thread: thread.map(str::to_string),
            seed: None,
            working: false,
            needs_attention: false,
            attention_reason: None,
            composer: None,
            parent: None,
        }
    }

    fn transcript(id: &str, updated_at: &str) -> crate::model::ResumeCandidate {
        crate::model::ResumeCandidate {
            id: id.into(),
            kind: AgentKind::Claude,
            source_path: format!("/home/me/.claude/projects/-home-me-work/{id}.jsonl"),
            recap: None,
            first_message: None,
            last_message: None,
            updated_at: updated_at.into(),
        }
    }

    #[test]
    fn two_sessions_in_one_folder_get_their_own_conversations() {
        // Both agents are running in the same repository, so both see both
        // transcripts. Only the daemon knows which is which.
        let files = vec![
            transcript("older", "2026-08-24T09:00:00Z"),
            transcript("newer", "2026-08-25T09:00:00Z"),
        ];
        let sessions = [
            live_session("s-one", Some("newer")),
            live_session("s-two", Some("older")),
        ];
        let mut spoken_for: HashMap<String, String> = sessions
            .iter()
            .filter_map(|s| s.thread.clone().map(|thread| (thread, s.id.clone())))
            .collect();
        let mut picks = Vec::new();
        for session in &sessions {
            let record = BackupRecord::default();
            let pick = pick_native_candidate(&files, session, &record, &spoken_for).unwrap();
            spoken_for.insert(pick.id.clone(), session.id.clone());
            picks.push(pick.id.clone());
        }
        assert_eq!(picks, vec!["newer".to_string(), "older".to_string()]);
    }

    #[test]
    fn the_daemon_can_take_a_session_off_its_neighbours_conversation() {
        // What a bad pairing left behind: this record has been mirroring the
        // transcript that belongs to the session next to it.
        let files = vec![
            transcript("mine", "2026-08-24T09:00:00Z"),
            transcript("theirs", "2026-08-25T09:00:00Z"),
        ];
        let session = live_session("s-one", Some("mine"));
        let record = BackupRecord {
            native_id: "theirs".into(),
            ..Default::default()
        };
        let spoken_for = HashMap::from([("theirs".to_string(), "s-two".to_string())]);
        let pick = pick_native_candidate(&files, &session, &record, &spoken_for).unwrap();
        assert_eq!(pick.id, "mine");
    }

    #[test]
    fn a_session_the_daemon_did_not_match_keeps_off_what_is_taken() {
        let files = vec![
            transcript("free", "2026-08-24T09:00:00Z"),
            transcript("taken", "2026-08-25T09:00:00Z"),
        ];
        let session = live_session("s-one", None);
        let spoken_for = HashMap::from([("taken".to_string(), "s-two".to_string())]);
        // The newest file in the folder is the other session's; guessing takes
        // the one that is not.
        let pick =
            pick_native_candidate(&files, &session, &BackupRecord::default(), &spoken_for).unwrap();
        assert_eq!(pick.id, "free");
        // And when every file is spoken for, it mirrors nothing rather than
        // somebody else's conversation.
        let all_taken = HashMap::from([
            ("taken".to_string(), "s-two".to_string()),
            ("free".to_string(), "s-three".to_string()),
        ]);
        assert!(
            pick_native_candidate(&files, &session, &BackupRecord::default(), &all_taken).is_none()
        );
    }

    /// How a resume came to reopen a stranger's conversation. A fresh agent in
    /// a folder somebody else had been working in got handed the file they
    /// left behind, mirrored it, and the archive kept the pairing.
    #[test]
    fn a_guess_never_reaches_back_before_the_session_started() {
        let files = vec![
            transcript("last-week", "2026-08-18T09:00:00.000Z"),
            transcript("ours", "2026-08-25T12:00:00.000Z"),
        ];
        let session = crate::daemon_protocol::DaemonSession {
            created_at: 1_787_652_000, // 2026-08-25T10:00:00Z
            ..live_session("s-one", None)
        };
        let record = BackupRecord::default();
        let pick = pick_native_candidate(&files, &session, &record, &HashMap::new()).unwrap();
        assert_eq!(pick.id, "ours");
        // And with only the stale one to choose from it mirrors nothing:
        // somebody else's id is worse than no id at all.
        assert!(pick_native_candidate(&files[..1], &session, &record, &HashMap::new()).is_none());
    }

    #[test]
    fn a_record_stays_on_its_transcript_when_no_one_can_name_one() {
        // An older companion reports no thread at all; the id this record
        // settled on before is still the best answer there is.
        let files = vec![
            transcript("mine", "2026-08-24T09:00:00Z"),
            transcript("newest", "2026-08-25T09:00:00Z"),
        ];
        let session = live_session("s-one", None);
        let record = BackupRecord {
            native_id: "mine".into(),
            ..Default::default()
        };
        let pick = pick_native_candidate(&files, &session, &record, &HashMap::new()).unwrap();
        assert_eq!(pick.id, "mine");
    }

    #[test]
    fn recoverable_records_are_the_history_the_machine_no_longer_has() {
        let store = temp_store();
        let mut index = BackupIndex::default();
        index.machines.push(MachineIdentity {
            key: "h20".into(),
            aliases: vec!["h20".into(), "h20-alt".into()],
            ..Default::default()
        });
        let mut add = |session: &str, native: &str, created: u64, kind: &str| {
            index.upsert(BackupRecord {
                target_id: "h20".into(),
                session_id: session.into(),
                kind: kind.into(),
                created_at: created,
                native_id: native.into(),
                native_path: format!("/home/me/.codex/sessions/2026/08/09/rollout-{native}.jsonl"),
                jsonl_bytes_synced: 10,
                message_count: 2,
                ..Default::default()
            });
        };
        add("muxloomd-codex-live", "native-live", 100, "codex");
        add("muxloomd-codex-old", "native-live", 90, "codex");
        add("muxloomd-codex-lost", "native-lost", 80, "codex");
        add("muxloomd-codex-dupe", "native-lost", 70, "codex");
        add("muxloomd-term", "native-term", 60, "terminal");
        // Backed up, but nothing readable came with it.
        index.upsert(BackupRecord {
            target_id: "h20".into(),
            session_id: "muxloomd-codex-empty".into(),
            kind: "codex".into(),
            created_at: 50,
            ..Default::default()
        });
        // Another machine's history must never appear under this one.
        index.upsert(BackupRecord {
            target_id: "local".into(),
            session_id: "muxloomd-codex-elsewhere".into(),
            kind: "codex".into(),
            created_at: 40,
            native_id: "native-elsewhere".into(),
            native_path: "/home/me/.codex/sessions/x.jsonl".into(),
            jsonl_bytes_synced: 10,
            message_count: 1,
            ..Default::default()
        });
        store.save_index(&index).unwrap();

        let live = HashSet::from(["muxloomd-codex-live".to_string()]);
        // Asked by the alias the machine is reachable under today, not the key
        // its records were partitioned under.
        let found = recoverable_records(&store, "h20-alt", &live);
        let ids: Vec<&str> = found
            .iter()
            .map(|record| record.session_id.as_str())
            .collect();
        // `-old` shares its transcript with the live session, so that
        // conversation is not lost; `-dupe` collapses into the newer `-lost`;
        // the terminal and the empty record carry nothing to show.
        assert_eq!(ids, ["muxloomd-codex-lost"]);
        assert!(is_restorable(&found[0]));

        // A machine that reports nothing at all has every conversation listed.
        let all = recoverable_records(&store, "h20", &HashSet::new());
        let ids: Vec<&str> = all
            .iter()
            .map(|record| record.session_id.as_str())
            .collect();
        assert_eq!(ids, ["muxloomd-codex-live", "muxloomd-codex-lost"]);
    }

    #[test]
    fn a_capture_only_record_is_listed_but_not_restorable() {
        let store = temp_store();
        store
            .append_frame("h20", "muxloomd-claude-capture", CAPTURE_BLOB, b"screen")
            .unwrap();
        let mut index = BackupIndex::default();
        index.upsert(BackupRecord {
            target_id: "h20".into(),
            session_id: "muxloomd-claude-capture".into(),
            kind: "claude".into(),
            created_at: 10,
            ..Default::default()
        });
        store.save_index(&index).unwrap();

        let found = recoverable_records(&store, "h20", &HashSet::new());
        assert_eq!(found.len(), 1, "a capture alone is still worth showing");
        assert!(!is_restorable(&found[0]));

        // And restoring it says so rather than writing an unusable file.
        let runtime = Runtime::new(&crate::config::Config::default());
        let error = restore_transcript(&runtime, &store, &Target::local(), &found[0])
            .expect_err("a capture cannot be resumed");
        assert!(
            error.to_string().contains("terminal output"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn native_paths_are_rebuilt_relative_to_the_target_home() {
        assert_eq!(
            native_relative_path("/Users/me/.claude/projects/slug/abc.jsonl").as_deref(),
            Some(".claude/projects/slug/abc.jsonl")
        );
        assert_eq!(
            native_relative_path("/home/other/.codex/sessions/2026/08/09/rollout-x.jsonl")
                .as_deref(),
            Some(".codex/sessions/2026/08/09/rollout-x.jsonl")
        );
        // Anything outside a known agent directory is not placed by guesswork.
        assert_eq!(native_relative_path("/var/tmp/notes.jsonl"), None);
        assert_eq!(native_relative_path(""), None);
    }

    #[test]
    fn restoring_an_unrecognised_path_refuses_rather_than_guessing() {
        let store = temp_store();
        let runtime = Runtime::new(&crate::config::Config::default());
        let record = BackupRecord {
            target_id: "h20".into(),
            session_id: "muxloomd-codex-odd".into(),
            kind: "codex".into(),
            native_id: "native-odd".into(),
            native_path: "/var/tmp/rollout-native-odd.jsonl".into(),
            jsonl_bytes_synced: 10,
            ..Default::default()
        };
        let error = restore_transcript(&runtime, &store, &Target::local(), &record)
            .expect_err("an unknown location must not be invented");
        assert!(
            error.to_string().contains("unrecognised path"),
            "unexpected error: {error}"
        );
    }
}
