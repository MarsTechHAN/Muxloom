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
    model::{AgentKind, LOCAL_TARGET_ID, Target, Transport},
    runtime::{Runtime, is_temporary_session_id},
};

/// zstd level for at-rest compression. Level 12 already crushes the redundant
/// terminal/jsonl input (hundreds of x) while staying fast enough to compress a
/// multi-MB capture delta in well under a second.
const ZSTD_LEVEL: i32 = 12;

/// Blob file names within a session directory.
pub const CAPTURE_BLOB: &str = "capture.ansi.zst";
pub const TRANSCRIPT_BLOB: &str = "transcript.jsonl.zst";
pub const MESSAGES_BLOB: &str = "messages.zst";

/// One backed-up session. Every field is `#[serde(default)]` so an index
/// written by an older build still loads as the schema grows.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
#[serde(default)]
pub struct BackupRecord {
    pub target_id: String,
    pub session_id: String,
    /// Agent kind as a string (`codex` | `claude` | `terminal`).
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
        // Collect the byte span of each whole frame.
        let mut frames: Vec<(usize, usize)> = Vec::new();
        let mut pos = 0usize;
        while pos + FRAME_HEADER_LEN <= bytes.len() {
            let comp_len =
                u64::from_le_bytes(bytes[pos + 8..pos + 16].try_into().unwrap()) as usize;
            let end = pos + FRAME_HEADER_LEN + comp_len;
            if end > bytes.len() {
                break;
            }
            frames.push((pos, end));
            pos = end;
        }
        let mut total: usize = frames.iter().map(|(start, end)| end - start).sum();
        let mut drop_to = 0usize;
        while total > max_bytes as usize && drop_to + 1 < frames.len() {
            let (start, end) = frames[drop_to];
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

        if let Ok(kind) = session.kind.parse::<AgentKind>()
            && kind != AgentKind::Terminal
            && let Err(error) = sync_transcript(
                runtime,
                store,
                target,
                &partition,
                is_local,
                kind,
                &session,
                &mut record,
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
        .read_history(target, session_id.to_string(), 0, 1)?
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
        let page = bridges.read_history(target, session_id.to_string(), offset, window)?;
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
/// jsonl). Local files are read directly; remote files are pulled over the
/// bridge. Skips work when the transcript's update marker is unchanged.
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
    stats: &mut SyncSummary,
) -> Result<()> {
    let candidates = runtime.scan_resumes(target, kind, &session.path)?;
    let Some(candidate) = pick_native_candidate(&candidates, record) else {
        return Ok(());
    };
    record.native_id = candidate.id.clone();
    record.native_path = candidate.source_path.clone();
    // Nothing to do if the transcript has not advanced since the last sync.
    if !candidate.updated_at.is_empty()
        && candidate.updated_at == record.native_updated_at
        && record.jsonl_bytes_synced > 0
    {
        return Ok(());
    }
    let updated_at = candidate.updated_at.clone();
    let source_path = candidate.source_path.clone();

    let data = if is_local {
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
    if record.title.is_empty()
        && let Some(title) = title
    {
        record.title = title;
    }
    stats.transcripts += 1;
    Ok(())
}

/// Choose the native transcript for a session. Once resolved we keep the same
/// id (stable); otherwise pick the most recently active candidate in that cwd.
/// (A precise start-time join is a later refinement — many sessions can share a
/// cwd.)
fn pick_native_candidate<'a>(
    candidates: &'a [crate::model::ResumeCandidate],
    record: &BackupRecord,
) -> Option<&'a crate::model::ResumeCandidate> {
    if !record.native_id.is_empty()
        && let Some(existing) = candidates.iter().find(|c| c.id == record.native_id)
    {
        return Some(existing);
    }
    candidates
        .iter()
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

/// Parse an agent-native transcript into a flat message list plus an optional
/// title. Best-effort: unknown line shapes are skipped, consecutive exact
/// duplicates (Codex emits both an `event_msg` and a `response_item` for the
/// same user turn) are collapsed.
pub fn extract_messages(kind: AgentKind, jsonl: &[u8]) -> (Vec<ExtractedMessage>, Option<String>) {
    let text = String::from_utf8_lossy(jsonl);
    let mut messages: Vec<ExtractedMessage> = Vec::new();
    let mut title: Option<String> = None;
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
                if title.is_none()
                    && let Some(explicit) = value
                        .get("summary")
                        .and_then(Value::as_str)
                        .or_else(|| value.get("customTitle").and_then(Value::as_str))
                {
                    title = Some(explicit.to_string());
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
            AgentKind::Terminal => {}
        }
    }
    if title.is_none() {
        title = messages
            .iter()
            .find(|message| message.role == "user")
            .map(|message| truncate_title(&message.text));
    }
    (messages, title)
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
}

/// Full-text search across every backed-up conversation (all machines, running
/// and archived). Case-insensitive substring match over extracted messages and
/// titles; results are ranked by match count then recency and capped at `limit`.
pub fn search(store: &BackupStore, query: &str, limit: usize) -> Result<Vec<SearchHit>> {
    let needle = query.trim().to_lowercase();
    if needle.is_empty() {
        return Ok(Vec::new());
    }
    let index = store.load_index()?;
    let mut hits: Vec<SearchHit> = Vec::new();
    for record in &index.records {
        let mut matched_message = false;
        let raw = store
            .read_blob(&record.target_id, &record.session_id, MESSAGES_BLOB)
            .unwrap_or_default();
        for line in String::from_utf8_lossy(&raw).lines() {
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
                });
            }
        }
    }
    hits.sort_by(|a, b| b.score.cmp(&a.score).then(b.created_at.cmp(&a.created_at)));
    hits.truncate(limit);
    Ok(hits)
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
        assert_eq!(title.as_deref(), Some("how do I scroll?"));
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
        assert_eq!(title.as_deref(), Some("scrolling help"));
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
}
