use std::{sync::mpsc, thread};

use crate::{
    config::CommandConfig,
    debug,
    model::{
        AgentKind, AgentSession, DirectoryListing, HistoryMatch, HistoryPage, LaunchRequest, Probe,
        ResumeCandidate, SearchMatchKind, SearchResult, Target,
    },
    runtime::Runtime,
};

#[derive(Debug, Clone)]
pub struct ScanRequest {
    pub target: Target,
    pub codex_command: String,
    pub claude_command: String,
    pub environment: Vec<(String, String)>,
    pub attention_patterns: Vec<String>,
}

#[derive(Debug)]
pub enum Request {
    Scan(ScanRequest),
    Capture {
        target: Target,
        session_id: String,
        offset_from_bottom: usize,
        lines: usize,
        width: u16,
        height: u16,
    },
    Launch {
        request: LaunchRequest,
        command: CommandConfig,
        environment: Vec<(String, String)>,
    },
    Install {
        target: Target,
        kind: AgentKind,
        command: CommandConfig,
        environment: Vec<(String, String)>,
    },
    Kill {
        target: Target,
        session_id: String,
    },
    Search {
        query: String,
        sessions: Vec<(Target, AgentSession)>,
    },
    ListDirectory {
        target: Target,
        path: String,
    },
    ScanResumes {
        target: Target,
        kind: AgentKind,
        path: String,
    },
}

#[derive(Debug)]
pub enum Event {
    Scanned {
        target_id: String,
        result: Result<(Probe, Vec<AgentSession>), String>,
    },
    Captured {
        target_id: String,
        session_id: String,
        result: Result<HistoryPage, String>,
    },
    Launched {
        target_id: String,
        result: Result<String, String>,
    },
    Installed {
        target_id: String,
        kind: AgentKind,
        result: Result<String, String>,
    },
    Killed {
        target_id: String,
        result: Result<(), String>,
    },
    Searched {
        query: String,
        results: Vec<SearchResult>,
    },
    DirectoryListed {
        target_id: String,
        requested_path: String,
        result: Result<DirectoryListing, String>,
    },
    ResumesScanned {
        target_id: String,
        kind: AgentKind,
        path: String,
        result: Result<Vec<ResumeCandidate>, String>,
    },
}

pub struct Worker {
    pub requests: mpsc::Sender<Request>,
    pub events: mpsc::Receiver<Event>,
}

impl Worker {
    pub fn start(runtime: Runtime) -> Self {
        let (request_tx, request_rx) = mpsc::channel::<Request>();
        let (event_tx, event_rx) = mpsc::channel::<Event>();

        thread::spawn(move || {
            while let Ok(request) = request_rx.recv() {
                let runtime = runtime.clone();
                let events = event_tx.clone();
                thread::spawn(move || match request {
                    Request::Scan(request) => {
                        let target_id = request.target.id.clone();
                        let mut result = runtime
                            .probe_and_discover(
                                &request.target,
                                &request.codex_command,
                                &request.claude_command,
                                &request.environment,
                            )
                            .map_err(|error| error.to_string());
                        if let Ok((_, sessions)) = &mut result {
                            for session in sessions.iter_mut().filter(|session| {
                                !session.dead && session.kind != crate::model::AgentKind::Terminal
                            }) {
                                match runtime.detect_attention(
                                    &request.target,
                                    &session.id,
                                    session.kind,
                                    &request.attention_patterns,
                                ) {
                                    Ok(Some(reason)) => {
                                        session.needs_attention = true;
                                        session.attention_reason = Some(reason);
                                    }
                                    Ok(None) => {}
                                    Err(error) => debug::log(
                                        "worker",
                                        format!(
                                            "attention check failed session={}: {error}",
                                            session.id
                                        ),
                                    ),
                                }
                            }
                        }
                        if let Err(error) = &result {
                            debug::log(
                                "worker",
                                format!("scan failed target={target_id}: {error}"),
                            );
                        }
                        let _ = events.send(Event::Scanned { target_id, result });
                    }
                    Request::Capture {
                        target,
                        session_id,
                        offset_from_bottom,
                        lines,
                        width,
                        height,
                    } => {
                        let target_id = target.id.clone();
                        let result = runtime
                            .capture_page(
                                &target,
                                &session_id,
                                offset_from_bottom,
                                lines,
                                width,
                                height,
                            )
                            .map_err(|error| error.to_string());
                        if let Err(error) = &result {
                            debug::log(
                                "worker",
                                format!("capture failed session={session_id}: {error}"),
                            );
                        }
                        let _ = events.send(Event::Captured {
                            target_id,
                            session_id,
                            result,
                        });
                    }
                    Request::Launch {
                        request,
                        command,
                        environment,
                    } => {
                        let target_id = request.target.id.clone();
                        let result = runtime
                            .launch(&request, &command, &environment)
                            .map_err(|error| error.to_string());
                        if let Err(error) = &result {
                            debug::log(
                                "worker",
                                format!("launch failed target={target_id}: {error}"),
                            );
                        }
                        let _ = events.send(Event::Launched { target_id, result });
                    }
                    Request::Install {
                        target,
                        kind,
                        command,
                        environment,
                    } => {
                        let target_id = target.id.clone();
                        let result = runtime
                            .install_runtime(&target, kind, &command, &environment)
                            .map_err(|error| error.to_string());
                        if let Err(error) = &result {
                            debug::log(
                                "worker",
                                format!("install failed target={target_id} kind={kind}: {error}"),
                            );
                        }
                        let _ = events.send(Event::Installed {
                            target_id,
                            kind,
                            result,
                        });
                    }
                    Request::Kill { target, session_id } => {
                        let target_id = target.id.clone();
                        let result = runtime
                            .kill(&target, &session_id)
                            .map_err(|error| error.to_string());
                        if let Err(error) = &result {
                            debug::log(
                                "worker",
                                format!("kill failed target={target_id}: {error}"),
                            );
                        }
                        let _ = events.send(Event::Killed { target_id, result });
                    }
                    Request::Search { query, sessions } => {
                        let mut results = Vec::new();
                        let mut history_jobs = Vec::new();
                        for (target, session) in sessions {
                            if let Some((score, snippet)) = best_name_match(&session, &query) {
                                results.push((
                                    search_result(&session, SearchMatchKind::Name, snippet, None),
                                    score,
                                ));
                            } else {
                                history_jobs.push((target, session));
                            }
                        }
                        // Multiplex a bounded number of SSH/tmux searches at once. This keeps
                        // large fleets responsive without opening an unbounded connection burst.
                        for jobs in history_jobs.chunks(8) {
                            let batch = thread::scope(|scope| {
                                let mut handles = Vec::new();
                                for (target, session) in jobs {
                                    let runtime = runtime.clone();
                                    let target = target.clone();
                                    let session = session.clone();
                                    let query = query.clone();
                                    handles.push(scope.spawn(move || {
                                        search_session_history(&runtime, target, session, &query)
                                    }));
                                }
                                handles
                                    .into_iter()
                                    .filter_map(|handle| handle.join().ok().flatten())
                                    .collect::<Vec<_>>()
                            });
                            results.extend(batch);
                        }
                        results.sort_by(|left, right| {
                            right
                                .0
                                .match_kind
                                .cmp(&left.0.match_kind)
                                .then_with(|| left.1.cmp(&right.1))
                                .then_with(|| right.0.created_at.cmp(&left.0.created_at))
                                .then_with(|| left.0.target_id.cmp(&right.0.target_id))
                        });
                        results.truncate(100);
                        let results: Vec<_> =
                            results.into_iter().map(|(result, _)| result).collect();
                        debug::log(
                            "search",
                            format!("query completed results={}", results.len()),
                        );
                        let _ = events.send(Event::Searched { query, results });
                    }
                    Request::ListDirectory { target, path } => {
                        let target_id = target.id.clone();
                        let requested_path = path.clone();
                        let result = runtime
                            .list_directory(&target, &path)
                            .map_err(|error| error.to_string());
                        let _ = events.send(Event::DirectoryListed {
                            target_id,
                            requested_path,
                            result,
                        });
                    }
                    Request::ScanResumes { target, kind, path } => {
                        let target_id = target.id.clone();
                        let result = runtime
                            .scan_resumes(&target, kind, &path)
                            .map_err(|error| error.to_string());
                        let _ = events.send(Event::ResumesScanned {
                            target_id,
                            kind,
                            path,
                            result,
                        });
                    }
                });
            }
        });

        Self {
            requests: request_tx,
            events: event_rx,
        }
    }
}

fn best_name_match(session: &AgentSession, query: &str) -> Option<(usize, String)> {
    let mut candidates = Vec::new();
    if !session.label.trim().is_empty()
        && let Some(score) = search_match_score(&session.label, query)
    {
        candidates.push((score, session.label.clone()));
    }
    let display = session.display_label();
    if let Some(score) = search_match_score(display, query) {
        candidates.push((score.saturating_add(10), display.to_string()));
    }
    if let Some(score) = search_match_score(&session.path, query) {
        candidates.push((score.saturating_add(25), session.path.clone()));
    }
    candidates.into_iter().min_by_key(|(score, _)| *score)
}

fn search_session_history(
    runtime: &Runtime,
    target: Target,
    session: AgentSession,
    query: &str,
) -> Option<(SearchResult, usize)> {
    let matches = match runtime.search_history(&target, &session.id, query, 12) {
        Ok(matches) => matches,
        Err(error) => {
            debug::log(
                "search",
                format!("history search failed session={}: {error}", session.id),
            );
            return None;
        }
    };
    let (item, score) = best_history_match(&matches, query)?;
    let match_kind = if item.recap {
        SearchMatchKind::Recap
    } else {
        SearchMatchKind::History
    };
    Some((
        search_result(
            &session,
            match_kind,
            item.text.clone(),
            Some(item.line_number),
        ),
        score,
    ))
}

fn search_result(
    session: &AgentSession,
    match_kind: SearchMatchKind,
    snippet: String,
    line_number: Option<usize>,
) -> SearchResult {
    SearchResult {
        session_id: session.id.clone(),
        target_id: session.target_id.clone(),
        kind: session.kind,
        label: session.display_label().to_string(),
        path: session.path.clone(),
        match_kind,
        snippet,
        line_number,
        created_at: session.created_at,
        dead: session.dead,
    }
}

fn best_history_match<'a>(
    matches: &'a [HistoryMatch],
    query: &str,
) -> Option<(&'a HistoryMatch, usize)> {
    best_history_kind(matches, query, true).or_else(|| best_history_kind(matches, query, false))
}

fn best_history_kind<'a>(
    matches: &'a [HistoryMatch],
    query: &str,
    recap: bool,
) -> Option<(&'a HistoryMatch, usize)> {
    matches
        .iter()
        .filter(|item| item.recap == recap)
        .filter_map(|item| search_match_score(&item.text, query).map(|score| (item, score)))
        .min_by(|(left, left_score), (right, right_score)| {
            left_score
                .cmp(right_score)
                .then_with(|| right.line_number.cmp(&left.line_number))
        })
}

fn search_match_score(value: &str, query: &str) -> Option<usize> {
    let value = value.to_lowercase();
    let query = query.trim().to_lowercase();
    if query.is_empty() {
        return None;
    }
    if value == query {
        return Some(0);
    }
    if value.starts_with(&query) {
        return Some(1 + value.len().saturating_sub(query.len()));
    }
    if let Some(position) = value.find(&query) {
        let word_boundary = position == 0
            || value[..position]
                .chars()
                .next_back()
                .is_none_or(|character| !character.is_alphanumeric());
        return Some(if word_boundary { 20 } else { 100 } + position);
    }

    let mut positions = Vec::new();
    for term in query.split_whitespace() {
        positions.push(value.find(term)?);
    }
    let first = positions.iter().copied().min().unwrap_or_default();
    let last = positions.iter().copied().max().unwrap_or_default();
    Some(500 + first + last.saturating_sub(first))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn search_ranking_prefers_exact_prefix_and_compact_multi_term_matches() {
        assert!(
            search_match_score("renderer", "renderer")
                < search_match_score("renderer work", "render")
        );
        assert!(search_match_score("fix remote renderer", "remote fix").is_some());
        assert!(search_match_score("unrelated", "remote fix").is_none());
    }

    #[test]
    fn recap_is_preferred_and_newer_equal_history_wins() {
        let matches = vec![
            HistoryMatch {
                recap: false,
                line_number: 10,
                text: "fix renderer".into(),
            },
            HistoryMatch {
                recap: true,
                line_number: 2,
                text: "fix renderer".into(),
            },
        ];
        assert!(best_history_match(&matches, "renderer").unwrap().0.recap);
    }
}
