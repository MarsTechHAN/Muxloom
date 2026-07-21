use std::{sync::mpsc, thread};

use crate::{
    config::CommandConfig,
    debug,
    model::{
        AgentKind, AgentSession, DirectoryListing, HistoryPage, LaunchRequest, Probe,
        ResumeCandidate, SearchMatchKind, SearchResult, Target,
    },
    runtime::Runtime,
};

#[derive(Debug, Clone)]
pub struct ScanRequest {
    pub target: Target,
    pub codex_command: String,
    pub claude_command: String,
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
        session_id: String,
        result: Result<HistoryPage, String>,
    },
    Launched {
        target_id: String,
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
                        let _ = events.send(Event::Captured { session_id, result });
                    }
                    Request::Launch { request, command } => {
                        let target_id = request.target.id.clone();
                        let result = runtime
                            .launch(&request, &command)
                            .map_err(|error| error.to_string());
                        if let Err(error) = &result {
                            debug::log(
                                "worker",
                                format!("launch failed target={target_id}: {error}"),
                            );
                        }
                        let _ = events.send(Event::Launched { target_id, result });
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
                        let lowered = query.to_lowercase();
                        let mut results = Vec::new();
                        for (target, session) in sessions {
                            let label = session.display_label().to_string();
                            let name_match = label.to_lowercase().contains(&lowered)
                                || session.path.to_lowercase().contains(&lowered);
                            let matches =
                                match runtime.search_history(&target, &session.id, &query, 8) {
                                    Ok(matches) => matches,
                                    Err(error) => {
                                        debug::log(
                                            "search",
                                            format!(
                                                "history search failed session={}: {error}",
                                                session.id
                                            ),
                                        );
                                        Vec::new()
                                    }
                                };
                            let best_match = matches
                                .iter()
                                .find(|item| item.recap)
                                .or_else(|| matches.first());
                            let (match_kind, snippet, line_number) = if name_match {
                                (SearchMatchKind::Name, label.clone(), None)
                            } else if let Some(item) = best_match {
                                (
                                    if item.recap {
                                        SearchMatchKind::Recap
                                    } else {
                                        SearchMatchKind::History
                                    },
                                    item.text.clone(),
                                    Some(item.line_number),
                                )
                            } else {
                                continue;
                            };
                            results.push(SearchResult {
                                session_id: session.id,
                                target_id: session.target_id,
                                kind: session.kind,
                                label,
                                path: session.path,
                                match_kind,
                                snippet,
                                line_number,
                                created_at: session.created_at,
                                dead: session.dead,
                            });
                        }
                        results.sort_by(|left, right| {
                            right
                                .match_kind
                                .cmp(&left.match_kind)
                                .then_with(|| right.created_at.cmp(&left.created_at))
                                .then_with(|| left.target_id.cmp(&right.target_id))
                        });
                        results.truncate(100);
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
