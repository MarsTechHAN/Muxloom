use std::{
    collections::HashMap,
    process::{Command, Output, Stdio},
    str::FromStr,
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, anyhow, bail};
use serde_json::Value;

use crate::{
    config::{CommandConfig, Config},
    debug,
    model::{
        AgentKind, AgentSession, DirectoryListing, HistoryMatch, HistoryPage, LaunchRequest, Probe,
        ResumeCandidate, Target, Transport,
    },
};

const SESSION_PREFIX: &str = "muxloom-";
const LEGACY_SESSION_PREFIX: &str = "ad-";
const FORMAT: &str = "#{session_name}\t#{@muxloom_kind}\t#{@muxloom_path}\t#{@muxloom_label}\t#{@muxloom_created}\t#{@agentdeck_kind}\t#{@agentdeck_path}\t#{@agentdeck_label}\t#{@agentdeck_created}\t#{pane_dead}\t#{pane_pid}";
static SESSION_COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone)]
pub struct Runtime {
    ssh_connect_timeout_secs: u64,
    history_limit: usize,
}

impl Runtime {
    pub fn new(config: &Config) -> Self {
        Self {
            ssh_connect_timeout_secs: config.ssh_connect_timeout_secs,
            history_limit: config.history_limit.max(2_000),
        }
    }

    pub fn probe_and_discover(
        &self,
        target: &Target,
        codex_command: &str,
        claude_command: &str,
    ) -> Result<(Probe, Vec<AgentSession>)> {
        debug::log("runtime", format!("probe start target={}", target.id));
        let codex_probe = login_shell_command(&format!(
            "command -v {} >/dev/null 2>&1",
            shell_quote(codex_command)
        ));
        let claude_probe = login_shell_command(&format!(
            "command -v {} >/dev/null 2>&1",
            shell_quote(claude_command)
        ));
        let probe = format!(
            "if {codex_probe} >/dev/null 2>&1; then printf 'codex=1\\n'; else printf 'codex=0\\n'; fi; \
             if {claude_probe} >/dev/null 2>&1; then printf 'claude=1\\n'; else printf 'claude=0\\n'; fi; \
             if command -v tmux >/dev/null 2>&1; then printf 'tmux=1\\n'; else printf 'tmux=0\\n'; fi",
        );
        let script = format!(
            "{}; {}",
            probe,
            shell_join(&[
                "tmux",
                "list-panes",
                "-a",
                "-F",
                FORMAT,
                "-f",
                "#{m/r:^(muxloom-|ad-),#{session_name}}",
            ]) + " 2>/dev/null || true"
        );
        let output = self.run_shell(target, &script, false)?;
        ensure_success(&output, "target probe")?;
        let result = parse_discovery(&target.id, &String::from_utf8_lossy(&output.stdout));
        if let Ok((probe, sessions)) = &result {
            debug::log(
                "runtime",
                format!(
                    "probe done target={} tmux={} codex={} claude={} sessions={}",
                    target.id,
                    probe.tmux,
                    probe.codex,
                    probe.claude,
                    sessions.len()
                ),
            );
        }
        result
    }

    pub fn launch(&self, request: &LaunchRequest, command: &CommandConfig) -> Result<String> {
        if request.path.trim().is_empty() {
            bail!("working directory cannot be empty");
        }
        if command.command.trim().is_empty() && request.kind != AgentKind::Terminal {
            bail!("command for {} is empty", request.kind);
        }

        debug::log(
            "runtime",
            format!(
                "launch start target={} kind={} path={} executable={}",
                request.target.id, request.kind, request.path, command.command
            ),
        );
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let sequence = SESSION_COUNTER.fetch_add(1, Ordering::Relaxed);
        let session_id = format!(
            "{SESSION_PREFIX}{}-{now}-{}-{sequence}",
            request.kind.as_str(),
            std::process::id()
        );
        let agent_command =
            if request.kind == AgentKind::Terminal && command.command.trim().is_empty() {
                "exec \"${SHELL:-/bin/sh}\" -l".into()
            } else {
                interactive_shell_command(&command_line(
                    command,
                    request.kind,
                    request.resume_id.as_deref(),
                ))
            };
        let label = request.label.replace(['\t', '\n', '\r'], " ");
        let metadata_path = request.path.replace(['\t', '\n', '\r'], " ");
        let bootstrap = format!("{session_id}-bootstrap");
        let agent_target = format!("{session_id}:agent");

        let commands = [
            shell_join(&[
                "tmux",
                "new-session",
                "-d",
                "-s",
                &session_id,
                "-n",
                &bootstrap,
            ]),
            shell_join(&[
                "tmux",
                "set-option",
                "-t",
                &session_id,
                "history-limit",
                &self.history_limit.to_string(),
            ]),
            shell_join(&[
                "tmux",
                "new-window",
                "-a",
                "-d",
                "-t",
                &format!("{session_id}:"),
                "-n",
                "agent",
                "-c",
                &request.path,
            ]),
            shell_join(&[
                "tmux",
                "kill-window",
                "-t",
                &format!("{session_id}:{bootstrap}"),
            ]),
            shell_join(&[
                "tmux",
                "set-option",
                "-w",
                "-t",
                &agent_target,
                "remain-on-exit",
                "on",
            ]),
            shell_join(&["tmux", "set-option", "-t", &session_id, "status", "off"]),
            shell_join(&["tmux", "set-option", "-t", &session_id, "mouse", "on"]),
            shell_join(&[
                "tmux",
                "set-option",
                "-t",
                &session_id,
                "@muxloom_kind",
                request.kind.as_str(),
            ]),
            shell_join(&[
                "tmux",
                "set-option",
                "-t",
                &session_id,
                "@muxloom_path",
                &metadata_path,
            ]),
            shell_join(&[
                "tmux",
                "set-option",
                "-t",
                &session_id,
                "@muxloom_label",
                &label,
            ]),
            shell_join(&[
                "tmux",
                "set-option",
                "-t",
                &session_id,
                "@muxloom_created",
                &now.to_string(),
            ]),
            shell_join(&[
                "tmux",
                "respawn-pane",
                "-k",
                "-t",
                &agent_target,
                &agent_command,
            ]),
        ];
        let script = commands.join(" && ");
        let output = self.run_shell(&request.target, &script, false)?;
        ensure_success(&output, "launch agent")?;
        debug::log(
            "runtime",
            format!(
                "launch done target={} session={session_id}",
                request.target.id
            ),
        );
        Ok(session_id)
    }

    pub fn capture_page(
        &self,
        target: &Target,
        session_id: &str,
        offset_from_bottom: usize,
        lines: usize,
        _width: u16,
        _height: u16,
    ) -> Result<HistoryPage> {
        validate_session_id(session_id)?;
        let lines = lines.max(1);
        let info = shell_join(&[
            "tmux",
            "display-message",
            "-p",
            "-t",
            session_id,
            "__AD_INFO__#{history_size}\t#{pane_height}\t#{pane_width}",
        ]);
        // Derive capture coordinates from the pane's actual height. History
        // reads must never resize the tmux window: doing so races the attached
        // PTY and produces the familiar vertical-bar/dot resize artifacts.
        let pane_height = shell_join(&[
            "tmux",
            "display-message",
            "-p",
            "-t",
            session_id,
            "#{pane_height}",
        ]);
        let capture = shell_join(&["tmux", "capture-pane", "-p", "-t", session_id]);
        let script = format!(
            "{info}; pane_height=$({pane_height}) || exit $?; \
             start=$((-{lines} - {offset_from_bottom})); \
             end=$((pane_height - 1 - {offset_from_bottom})); \
             {capture} -S \"$start\" -E \"$end\""
        );
        let output = self.run_shell(target, &script, false)?;
        ensure_success(&output, "capture recap")?;
        parse_history_page(&String::from_utf8_lossy(&output.stdout), offset_from_bottom)
    }

    pub fn capture(&self, target: &Target, session_id: &str, lines: usize) -> Result<String> {
        Ok(self
            .capture_page(target, session_id, 0, lines, 80, 24)?
            .text)
    }

    pub fn detect_attention(
        &self,
        target: &Target,
        session_id: &str,
        kind: AgentKind,
        patterns: &[String],
    ) -> Result<Option<String>> {
        validate_session_id(session_id)?;
        let script = shell_join(&["tmux", "capture-pane", "-p", "-t", session_id]);
        let output = self.run_shell(target, &script, false)?;
        ensure_success(&output, "inspect agent attention")?;
        let screen = String::from_utf8_lossy(&output.stdout);
        let reason = attention_reason(kind, &screen, patterns);
        if let Some(reason) = &reason {
            debug::log(
                "attention",
                format!(
                    "matched target={} session={} kind={} reason={} tail={}",
                    target.id,
                    session_id,
                    kind,
                    reason,
                    attention_debug_tail(&screen)
                ),
            );
        }
        Ok(reason)
    }

    pub fn search_history(
        &self,
        target: &Target,
        session_id: &str,
        query: &str,
        max_matches: usize,
    ) -> Result<Vec<HistoryMatch>> {
        validate_session_id(session_id)?;
        let query = query.trim();
        if query.is_empty() {
            return Ok(Vec::new());
        }
        let max_matches = max_matches.clamp(1, 50);
        let recap = shell_join(&["tmux", "capture-pane", "-p", "-J", "-t", session_id]);
        let history = shell_join(&[
            "tmux",
            "capture-pane",
            "-p",
            "-J",
            "-S",
            "-",
            "-t",
            session_id,
        ]);
        let awk_program = r#"index(tolower($0), tolower(q)) { printf "%s%d\t%s\n", prefix, NR, $0; if (++n >= limit) exit }"#;
        let awk_recap = shell_join(&[
            "awk",
            "-v",
            &format!("q={query}"),
            "-v",
            "prefix=__AD_RECAP__",
            "-v",
            &format!("limit={max_matches}"),
            awk_program,
        ]);
        let awk_history = shell_join(&[
            "awk",
            "-v",
            &format!("q={query}"),
            "-v",
            "prefix=__AD_HISTORY__",
            "-v",
            &format!("limit={max_matches}"),
            awk_program,
        ]);
        let script = format!("{recap} | {awk_recap}; {history} | {awk_history}");
        let output = self.run_shell(target, &script, false)?;
        ensure_success(&output, "search agent history")?;
        Ok(parse_history_matches(&String::from_utf8_lossy(
            &output.stdout,
        )))
    }

    pub fn list_directory(&self, target: &Target, path: &str) -> Result<DirectoryListing> {
        let path = if path.trim().is_empty() { "." } else { path };
        let script = format!(
            "cd {} && pwd -P && find -L . -mindepth 1 -maxdepth 1 -type d -print0",
            shell_quote(path)
        );
        let output = self.run_shell(target, &script, false)?;
        ensure_success(&output, "list directory")?;
        parse_directory_listing(&output.stdout)
    }

    pub fn scan_resumes(
        &self,
        target: &Target,
        kind: AgentKind,
        path: &str,
    ) -> Result<Vec<ResumeCandidate>> {
        if kind == AgentKind::Terminal {
            return Ok(Vec::new());
        }
        let root = match kind {
            AgentKind::Codex => "$HOME/.codex/sessions",
            AgentKind::Claude => "$HOME/.claude/projects",
            AgentKind::Terminal => unreachable!(),
        };
        let index = if kind == AgentKind::Codex {
            r#"printf '\036INDEX\n'; if [ -f "$HOME/.codex/session_index.jsonl" ]; then cat "$HOME/.codex/session_index.jsonl"; fi;"#
        } else {
            ""
        };
        let collect = r#"query=$1; shift; for file do if grep -F -q -- "$query" "$file"; then printf '\036SESSION\n'; sed -n '1,60p' "$file"; tail -n 80 "$file"; fi; done"#;
        let find_args = shell_join(&[
            "-type", "f", "-name", "*.jsonl", "-exec", "sh", "-c", collect, "sh", path, "{}", "+",
        ]);
        let find = format!("find \"{root}\" {find_args}");
        let script = format!("{index} if [ -d \"{root}\" ]; then {find}; fi");
        let output = self.run_shell(target, &script, false)?;
        ensure_success(&output, "scan resumable sessions")?;
        Ok(parse_resume_candidates(
            kind,
            path,
            &String::from_utf8_lossy(&output.stdout),
        ))
    }

    pub fn kill(&self, target: &Target, session_id: &str) -> Result<()> {
        debug::log(
            "runtime",
            format!("kill target={} session={session_id}", target.id),
        );
        validate_session_id(session_id)?;
        let script = shell_join(&["tmux", "kill-session", "-t", session_id]);
        let output = self.run_shell(target, &script, false)?;
        ensure_success(&output, "delete agent session")
    }

    pub fn attach(&self, target: &Target, session_id: &str) -> Result<()> {
        validate_session_id(session_id)?;
        let status = match &target.transport {
            Transport::Local => Command::new("tmux")
                .args(["attach-session", "-t", session_id])
                .status()
                .context("failed to run tmux")?,
            Transport::Ssh { alias } => Command::new("ssh")
                .args(["-t", alias, "tmux", "attach-session", "-t", session_id])
                .status()
                .with_context(|| format!("failed to run ssh for {alias}"))?,
        };
        if status.success() {
            Ok(())
        } else {
            bail!("attach exited with {status}")
        }
    }

    fn run_shell(&self, target: &Target, script: &str, interactive: bool) -> Result<Output> {
        let mut command = match &target.transport {
            Transport::Local => {
                let mut command = Command::new("sh");
                command.args(["-lc", script]);
                command
            }
            Transport::Ssh { alias } => {
                let mut command = Command::new("ssh");
                let control_path = ssh_control_path();
                let control_option = format!("ControlPath={control_path}");
                command.args([
                    "-o",
                    "BatchMode=yes",
                    "-o",
                    &format!("ConnectTimeout={}", self.ssh_connect_timeout_secs),
                    "-o",
                    "ControlMaster=auto",
                    "-o",
                    "ControlPersist=60",
                    "-o",
                    &control_option,
                    alias,
                    "sh",
                    "-lc",
                    &shell_quote(script),
                ]);
                command
            }
        };
        if !interactive {
            command.stdin(Stdio::null());
        }
        command
            .output()
            .with_context(|| format!("failed to execute command on {}", target.id))
    }
}

pub fn ssh_control_path() -> String {
    format!("/tmp/muxloom-{}-%C", std::process::id())
}

fn parse_history_page(output: &str, offset_from_bottom: usize) -> Result<HistoryPage> {
    let mut lines = output.splitn(2, '\n');
    let info = lines.next().unwrap_or_default();
    let Some(info) = info.strip_prefix("__AD_INFO__") else {
        bail!("tmux returned malformed history metadata");
    };
    let fields: Vec<_> = info.split('\t').collect();
    if fields.len() != 3 {
        bail!("tmux returned incomplete history metadata");
    }
    Ok(HistoryPage {
        text: lines.next().unwrap_or_default().trim_end().to_string(),
        history_size: fields[0].parse().unwrap_or(0),
        pane_height: fields[1].parse().unwrap_or(0),
        pane_width: fields[2].parse().unwrap_or(0),
        offset_from_bottom,
    })
}

fn attention_reason(kind: AgentKind, screen: &str, patterns: &[String]) -> Option<String> {
    let screen = attention_tail(screen).to_lowercase();
    if let Some(pattern) = patterns.iter().find(|pattern| {
        let pattern = pattern.trim();
        !pattern.is_empty() && screen.contains(&pattern.to_lowercase())
    }) {
        return Some(pattern.clone());
    }

    let has_yes = screen.lines().any(|line| choice_line(line, "yes"));
    let has_no = screen.lines().any(|line| choice_line(line, "no"));
    let has_allow = screen.lines().any(|line| choice_line(line, "allow"));
    let has_deny = screen.lines().any(|line| {
        choice_line(line, "deny") || choice_line(line, "reject") || choice_line(line, "cancel")
    });
    let has_choice = (has_yes && has_no)
        || (has_allow && has_deny)
        || (has_yes && (screen.contains("esc to cancel") || screen.contains("enter to confirm")));
    let builtins: &[(&str, &[&str])] = match kind {
        AgentKind::Codex => &[
            (
                "command approval",
                &["run this command", "run the following command"],
            ),
            (
                "file change approval",
                &["apply this patch", "make this change"],
            ),
            (
                "confirmation",
                &["press enter to confirm", "enter to confirm"],
            ),
        ],
        AgentKind::Claude => &[
            (
                "permission request",
                &["allow this", "allow command", "permission"],
            ),
            ("confirmation", &["do you want to proceed", "esc to cancel"]),
        ],
        AgentKind::Terminal => &[],
    };
    for (reason, markers) in builtins {
        if markers.iter().any(|marker| screen.contains(marker)) && has_choice {
            return Some((*reason).into());
        }
    }
    if has_choice
        && [
            "would you like",
            "do you want",
            "choose an option",
            "select an option",
            "permission",
        ]
        .iter()
        .any(|marker| screen.contains(marker))
    {
        return Some("interactive choice".into());
    }
    None
}

fn attention_tail(screen: &str) -> String {
    let lines: Vec<_> = screen.lines().collect();
    lines[lines.len().saturating_sub(24)..].join("\n")
}

fn attention_debug_tail(screen: &str) -> String {
    let lines: Vec<_> = screen.lines().collect();
    lines[lines.len().saturating_sub(10)..]
        .iter()
        .map(|line| line.trim())
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join(" | ")
        .chars()
        .take(600)
        .collect()
}

fn choice_line(line: &str, label: &str) -> bool {
    if line.chars().count() > 120 {
        return false;
    }
    let value = line.trim_start_matches(|character: char| {
        character.is_whitespace() || matches!(character, '›' | '❯' | '>' | '•' | '*' | '-')
    });
    let value = value.trim_start_matches(|character: char| {
        character.is_whitespace()
            || character.is_ascii_digit()
            || matches!(character, '.' | ')' | '(' | '[' | ']')
    });
    value.strip_prefix(label).is_some_and(|rest| {
        rest.chars()
            .next()
            .is_none_or(|character| !character.is_ascii_alphanumeric())
    })
}

fn parse_history_matches(output: &str) -> Vec<HistoryMatch> {
    output
        .lines()
        .filter_map(|line| {
            let (recap, rest) = if let Some(rest) = line.strip_prefix("__AD_RECAP__") {
                (true, rest)
            } else {
                (false, line.strip_prefix("__AD_HISTORY__")?)
            };
            let (number, text) = rest.split_once('\t')?;
            Some(HistoryMatch {
                recap,
                line_number: number.parse().ok()?,
                text: sanitize_field(text),
            })
        })
        .collect()
}

fn parse_directory_listing(output: &[u8]) -> Result<DirectoryListing> {
    let Some(newline) = output.iter().position(|byte| *byte == b'\n') else {
        bail!("directory listing did not include its canonical path");
    };
    let path = String::from_utf8_lossy(&output[..newline])
        .trim()
        .to_string();
    let mut directories: Vec<_> = output[newline + 1..]
        .split(|byte| *byte == 0)
        .filter(|entry| !entry.is_empty())
        .map(|entry| String::from_utf8_lossy(entry).to_string())
        .filter_map(|entry| entry.strip_prefix("./").map(str::to_string))
        .filter(|entry| !entry.is_empty() && !entry.contains('/'))
        .collect();
    directories.sort_by_key(|entry| entry.to_lowercase());
    directories.dedup();
    Ok(DirectoryListing { path, directories })
}

fn parse_resume_candidates(kind: AgentKind, path: &str, output: &str) -> Vec<ResumeCandidate> {
    let mut titles = HashMap::new();
    let chunks: Vec<_> = output.split('\u{1e}').collect();
    for chunk in &chunks {
        let Some(index) = chunk.strip_prefix("INDEX\n") else {
            continue;
        };
        for value in index.lines().filter_map(parse_json_line) {
            if let (Some(id), Some(title)) = (
                value.get("id").and_then(Value::as_str),
                value.get("thread_name").and_then(Value::as_str),
            ) {
                titles.insert(id.to_string(), title.to_string());
            }
        }
    }

    let normalized_path = normalize_path(path);
    let mut candidates = HashMap::<String, ResumeCandidate>::new();
    for chunk in chunks {
        let Some(session) = chunk.strip_prefix("SESSION\n") else {
            continue;
        };
        let candidate = match kind {
            AgentKind::Codex => parse_codex_resume(session, &normalized_path, &titles),
            AgentKind::Claude => parse_claude_resume(session, &normalized_path),
            AgentKind::Terminal => None,
        };
        if let Some(candidate) = candidate {
            candidates
                .entry(candidate.id.clone())
                .and_modify(|existing| {
                    if candidate.updated_at > existing.updated_at {
                        *existing = candidate.clone();
                    }
                })
                .or_insert(candidate);
        }
    }
    let mut candidates: Vec<_> = candidates.into_values().collect();
    candidates.sort_by(|left, right| right.updated_at.cmp(&left.updated_at));
    candidates.truncate(50);
    candidates
}

fn parse_codex_resume(
    session: &str,
    path: &str,
    titles: &HashMap<String, String>,
) -> Option<ResumeCandidate> {
    let mut id = None;
    let mut cwd = None;
    let mut updated_at = String::new();
    let mut first_message = None;
    let mut last_message = None;
    let mut fallback_first = None;
    let mut fallback_last = None;
    for value in session.lines().filter_map(parse_json_line) {
        match value.get("type").and_then(Value::as_str) {
            Some("session_meta") => {
                let payload = value.get("payload")?;
                id = payload
                    .get("id")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                cwd = payload
                    .get("cwd")
                    .and_then(Value::as_str)
                    .map(normalize_path);
                updated_at = payload
                    .get("timestamp")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
            }
            Some("event_msg") => {
                let payload = value.get("payload");
                if payload
                    .and_then(|payload| payload.get("type"))
                    .and_then(Value::as_str)
                    == Some("user_message")
                {
                    let message = payload
                        .and_then(|payload| payload.get("message"))
                        .and_then(Value::as_str)
                        .map(clean_recap)
                        .filter(|message| !message.is_empty());
                    if let Some(message) = message {
                        first_message.get_or_insert_with(|| message.clone());
                        last_message = Some(message);
                    }
                }
            }
            Some("response_item") => {
                let payload = value.get("payload");
                if payload
                    .and_then(|payload| payload.get("role"))
                    .and_then(Value::as_str)
                    == Some("user")
                {
                    let message = payload
                        .and_then(|payload| payload.get("content"))
                        .and_then(extract_message_text)
                        .map(|message| clean_recap(&message))
                        .filter(|message| {
                            !message.is_empty() && !message.starts_with("<environment_context>")
                        });
                    if let Some(message) = message {
                        fallback_first.get_or_insert_with(|| message.clone());
                        fallback_last = Some(message);
                    }
                }
            }
            _ => {}
        }
    }
    let id = id?;
    if cwd.as_deref() != Some(path) {
        return None;
    }
    let recap = titles
        .get(&id)
        .cloned()
        .map(|title| clean_recap(&title))
        .filter(|title| !title.is_empty());
    Some(ResumeCandidate {
        id,
        recap,
        first_message: first_message.or(fallback_first),
        last_message: last_message.or(fallback_last),
        updated_at,
    })
}

fn parse_claude_resume(session: &str, path: &str) -> Option<ResumeCandidate> {
    let mut id = None;
    let mut cwd = None;
    let mut updated_at = String::new();
    let mut first_message = None;
    let mut last_message = None;
    let mut summary = None;
    for value in session.lines().filter_map(parse_json_line) {
        if id.is_none() {
            id = value
                .get("sessionId")
                .and_then(Value::as_str)
                .map(str::to_string);
        }
        if cwd.is_none() {
            cwd = value.get("cwd").and_then(Value::as_str).map(normalize_path);
        }
        if let Some(timestamp) = value.get("timestamp").and_then(Value::as_str)
            && timestamp > updated_at.as_str()
        {
            updated_at = timestamp.to_string();
        }
        if summary.is_none() {
            summary = value
                .get("summary")
                .or_else(|| value.get("customTitle"))
                .and_then(Value::as_str)
                .map(clean_recap);
        }
        if value.get("type").and_then(Value::as_str) == Some("user") {
            let message = value
                .get("message")
                .and_then(|message| message.get("content"))
                .and_then(extract_message_text)
                .map(|message| clean_recap(&message))
                .filter(|message| !message.is_empty());
            if let Some(message) = message {
                first_message.get_or_insert_with(|| message.clone());
                last_message = Some(message);
            }
        }
    }
    if cwd.as_deref() != Some(path) {
        return None;
    }
    Some(ResumeCandidate {
        id: id?,
        recap: summary.filter(|message| !message.is_empty()),
        first_message,
        last_message,
        updated_at,
    })
}

fn parse_json_line(line: &str) -> Option<Value> {
    serde_json::from_str(line).ok()
}

fn extract_message_text(value: &Value) -> Option<String> {
    if let Some(text) = value.as_str() {
        return Some(text.to_string());
    }
    let parts = value.as_array()?.iter().filter_map(|part| {
        part.get("text")
            .or_else(|| part.get("content"))
            .and_then(Value::as_str)
    });
    Some(parts.collect::<Vec<_>>().join(" "))
}

fn clean_recap(value: &str) -> String {
    let flattened = value.split_whitespace().collect::<Vec<_>>().join(" ");
    flattened.chars().take(180).collect()
}

fn normalize_path(value: &str) -> String {
    if value == "/" {
        "/".into()
    } else {
        value.trim_end_matches('/').to_string()
    }
}

fn command_line(command: &CommandConfig, kind: AgentKind, resume_id: Option<&str>) -> String {
    let mut values = Vec::with_capacity(command.args.len() + 3);
    values.push(command.command.as_str());
    values.extend(command.args.iter().map(String::as_str));
    if let Some(resume_id) = resume_id {
        match kind {
            AgentKind::Codex => values.extend(["resume", resume_id]),
            AgentKind::Claude => values.extend(["--resume", resume_id]),
            AgentKind::Terminal => {}
        }
    }
    shell_join(&values)
}

fn interactive_shell_command(command: &str) -> String {
    format!("exec {}", login_shell_command(&format!("exec {command}")))
}

fn login_shell_command(command: &str) -> String {
    format!("\"${{SHELL:-/bin/sh}}\" -lc {}", shell_quote(command))
}

fn parse_discovery(target_id: &str, output: &str) -> Result<(Probe, Vec<AgentSession>)> {
    let mut probe = Probe::default();
    let mut sessions = Vec::new();
    for line in output.lines() {
        match line {
            "tmux=1" => probe.tmux = true,
            "codex=1" => probe.codex = true,
            "claude=1" => probe.claude = true,
            "tmux=0" | "codex=0" | "claude=0" => {}
            line if is_managed_session_id(line.split('\t').next().unwrap_or_default()) => {
                let fields: Vec<_> = line.split('\t').collect();
                if fields.len() < 11 {
                    continue;
                }
                let metadata = if fields[1].is_empty() {
                    (&fields[5..9], 9, 10)
                } else {
                    (&fields[1..5], 9, 10)
                };
                let Ok(kind) = AgentKind::from_str(metadata.0[0]) else {
                    continue;
                };
                sessions.push(AgentSession {
                    id: sanitize_field(fields[0]),
                    target_id: target_id.into(),
                    kind,
                    path: sanitize_field(metadata.0[1]),
                    label: sanitize_field(metadata.0[2]),
                    created_at: metadata.0[3].parse().unwrap_or(0),
                    dead: fields[metadata.1] == "1",
                    pid: fields[metadata.2].parse().ok(),
                    needs_attention: false,
                    attention_reason: None,
                });
            }
            _ => {}
        }
    }
    Ok((probe, sessions))
}

fn sanitize_field(value: &str) -> String {
    value
        .chars()
        .filter(|character| !character.is_control())
        .collect()
}

fn ensure_success(output: &Output, action: &str) -> Result<()> {
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    Err(anyhow!(
        "{action} failed ({}): {}",
        output.status,
        if stderr.is_empty() {
            "no error output"
        } else {
            &stderr
        }
    ))
}

fn validate_session_id(session_id: &str) -> Result<()> {
    if is_managed_session_id(session_id) {
        Ok(())
    } else {
        bail!("refusing invalid Muxloom session id")
    }
}

pub(crate) fn is_managed_session_id(session_id: &str) -> bool {
    (session_id.starts_with(SESSION_PREFIX) || session_id.starts_with(LEGACY_SESSION_PREFIX))
        && session_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
}

pub fn shell_join(values: &[&str]) -> String {
    values
        .iter()
        .map(|value| shell_quote(value))
        .collect::<Vec<_>>()
        .join(" ")
}

pub fn shell_quote(value: &str) -> String {
    if !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_@%+=:,./-".contains(&byte))
    {
        return value.to_string();
    }
    format!("'{}'", value.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quotes_shell_values() {
        assert_eq!(shell_quote("hello"), "hello");
        assert_eq!(shell_quote("two words"), "'two words'");
        assert_eq!(shell_quote("it's"), "'it'\\''s'");
        assert_eq!(shell_quote(""), "''");
    }

    #[test]
    fn parses_probe_and_sessions() {
        let output = concat!(
            "tmux=1\n",
            "codex=1\n",
            "claude=0\n",
            "muxloom-codex-10-2\tcodex\t/work/a b\talpha\t10\t\t\t\t\t0\t123\n",
            "ad-claude-11-2\t\t\t\t\tclaude\t/work/remote\tdone\t11\t1\t456\n"
        );
        let (probe, sessions) = parse_discovery("gpu", output).unwrap();
        assert!(probe.tmux && probe.codex && !probe.claude);
        assert_eq!(sessions.len(), 2);
        assert_eq!(sessions[0].path, "/work/a b");
        assert!(sessions[0].id.starts_with("muxloom-"));
        assert_eq!(sessions[0].target_id, "gpu");
        assert!(sessions[1].id.starts_with("ad-"));
        assert!(sessions[1].dead, "remote dead panes must be archived");
    }

    #[test]
    fn accepts_current_and_legacy_managed_session_ids() {
        assert!(is_managed_session_id("muxloom-codex-10-2"));
        assert!(is_managed_session_id("ad-claude-10-2"));
        assert!(!is_managed_session_id("other-codex-10-2"));
        assert!(!is_managed_session_id("muxloom-invalid/session"));
    }

    #[test]
    fn parses_paged_history_metadata() {
        let page = parse_history_page("__AD_INFO__120\t24\t80\nline one\nline two\n", 20).unwrap();
        assert_eq!(page.history_size, 120);
        assert_eq!(page.pane_height, 24);
        assert_eq!(page.pane_width, 80);
        assert_eq!(page.offset_from_bottom, 20);
        assert_eq!(page.text, "line one\nline two");
        assert!(page.has_older());
    }

    #[test]
    fn detects_runtime_attention_prompts() {
        let codex = "Would you like to run the following command?\n› 1. Yes\n  2. No\nPress enter to confirm";
        assert_eq!(
            attention_reason(AgentKind::Codex, codex, &[]).as_deref(),
            Some("command approval")
        );
        let claude = "Do you want to proceed?\n❯ 1. Yes\n  2. No\nEsc to cancel";
        assert_eq!(
            attention_reason(AgentKind::Claude, claude, &[]).as_deref(),
            Some("confirmation")
        );
        let idle_prompt = concat!(
            "Earlier output: 1. Yes\n",
            "Earlier output: 2. No\n",
            "Task completed successfully.\n",
            "› Explain this codebase\n",
            "gpt-5.6-sol max · /work/project\n"
        );
        assert_eq!(attention_reason(AgentKind::Codex, idle_prompt, &[]), None);

        let mut stale_prompt =
            String::from("Would you like to run the following command?\n› 1. Yes\n  2. No\n");
        stale_prompt.push_str(
            &(0..30)
                .map(|index| format!("working output {index}\n"))
                .collect::<String>(),
        );
        assert_eq!(attention_reason(AgentKind::Codex, &stale_prompt, &[]), None);
        assert!(attention_reason(AgentKind::Codex, "working...", &[]).is_none());
    }

    #[test]
    fn parses_rankable_history_matches() {
        let matches =
            parse_history_matches("__AD_RECAP__3\tapprove now\n__AD_HISTORY__91\tolder mention\n");
        assert_eq!(matches.len(), 2);
        assert!(matches[0].recap);
        assert_eq!(matches[1].line_number, 91);
    }

    #[test]
    fn parses_directory_entries_and_runtime_resume_metadata() {
        let listing =
            parse_directory_listing(b"/work/project\n./src\0./.hidden\0./tests\0").unwrap();
        assert_eq!(listing.path, "/work/project");
        assert_eq!(listing.directories, [".hidden", "src", "tests"]);

        let codex = concat!(
            "\u{1e}INDEX\n",
            "{\"id\":\"codex-id\",\"thread_name\":\"Fix the renderer\",\"updated_at\":\"2026-07-20T10:00:00Z\"}\n",
            "\u{1e}SESSION\n",
            "{\"type\":\"session_meta\",\"payload\":{\"id\":\"codex-id\",\"cwd\":\"/work/project\",\"timestamp\":\"2026-07-20T09:00:00Z\"}}\n",
            "{\"type\":\"event_msg\",\"payload\":{\"type\":\"user_message\",\"message\":\"first codex prompt\"}}\n"
        );
        let candidates = parse_resume_candidates(AgentKind::Codex, "/work/project/", codex);
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].id, "codex-id");
        assert_eq!(candidates[0].recap.as_deref(), Some("Fix the renderer"));
        assert_eq!(
            candidates[0].first_message.as_deref(),
            Some("first codex prompt")
        );
        assert_eq!(
            candidates[0].last_message.as_deref(),
            Some("first codex prompt")
        );

        let claude = concat!(
            "\u{1e}SESSION\n",
            "{\"type\":\"user\",\"sessionId\":\"claude-id\",\"cwd\":\"/work/project\",\"timestamp\":\"2026-07-20T11:00:00Z\",\"message\":{\"content\":\"first claude prompt\"}}\n"
        );
        let candidates = parse_resume_candidates(AgentKind::Claude, "/work/project", claude);
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].recap, None);
        assert_eq!(
            candidates[0].first_message.as_deref(),
            Some("first claude prompt")
        );
        assert_eq!(
            candidates[0].last_message.as_deref(),
            Some("first claude prompt")
        );
        assert!(
            parse_resume_candidates(AgentKind::Claude, "/other", claude).is_empty(),
            "resume candidates must match the exact working directory"
        );
    }

    #[test]
    fn builds_runtime_specific_resume_commands() {
        let command = CommandConfig {
            command: "codex".into(),
            args: vec!["--full-auto".into()],
        };
        assert_eq!(
            command_line(&command, AgentKind::Codex, Some("session id")),
            "codex --full-auto resume 'session id'"
        );
        let command = CommandConfig {
            command: "claude".into(),
            args: Vec::new(),
        };
        assert_eq!(
            command_line(&command, AgentKind::Claude, Some("abc")),
            "claude --resume abc"
        );
    }
}
