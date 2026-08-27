//! Minimal MCP (Model Context Protocol) server, spoken over any line-based
//! byte stream.
//!
//! Both binaries expose a control surface to AI agents through this module:
//! `muxloom mcp` serves every enabled machine and `muxloomd mcp` serves the
//! local daemon. The transport here is MCP's stdio framing — one JSON-RPC 2.0
//! message per line — but the loop is generic over [`BufRead`]/[`Write`], so a
//! TCP or serial adapter for a hardware status panel can reuse it unchanged.
//!
//! The implementation is deliberately hand-rolled on `serde_json`: the
//! codebase has no async runtime, and the protocol subset a tool server needs
//! (initialize, tools/list, tools/call, ping) is small enough that an SDK
//! would cost more than it saves.

use std::io::{BufRead, Write};
use std::sync::{Arc, Mutex};
use std::thread;

use anyhow::{Context, Result};
use serde_json::{Value, json};

use crate::control::ControlSurface;

/// The protocol revision answered when the client proposes one this server
/// does not know. Revisions are date strings, so proposals are otherwise
/// echoed back: the tool subset served here predates every revision.
const PROTOCOL_REVISION: &str = "2025-06-18";
const KNOWN_REVISIONS: [&str; 3] = ["2024-11-05", "2025-03-26", "2025-06-18"];

const PARSE_ERROR: i64 = -32700;
const INVALID_REQUEST: i64 = -32600;
const METHOD_NOT_FOUND: i64 = -32601;
const INVALID_PARAMS: i64 = -32602;

/// Serve MCP over the given transport until the client disconnects. `name` is
/// the serverInfo name a client shows the user, e.g. `muxloom` or `muxloomd`.
///
/// Each request is answered on its own thread so that a long-running call
/// (a `wait_for` or `talk_read` that blocks for minutes) cannot stall the
/// reader loop and starve every other in-flight request into a client-side
/// timeout. JSON-RPC responses carry their own `id`, so replies may arrive
/// out of order.
pub fn serve(
    surface: &dyn ControlSurface,
    name: &str,
    reader: impl BufRead,
    writer: impl Write + Send,
) -> Result<()> {
    let name = name.to_string();
    let writer = Arc::new(Mutex::new(writer));

    thread::scope(|scope| -> Result<()> {
        for result in reader.lines() {
            let line = result.context("failed to read MCP transport")?;
            if line.trim().is_empty() {
                continue;
            }
            let name = name.clone();
            let writer = Arc::clone(&writer);
            scope.spawn(move || {
                let Some(reply) = handle_line(surface, &name, &line) else {
                    return;
                };
                let Ok(mut bytes) = serde_json::to_vec(&reply) else {
                    return;
                };
                bytes.push(b'\n');
                if let Ok(mut guard) = writer.lock() {
                    let _ = guard.write_all(&bytes).and_then(|()| guard.flush());
                }
            });
        }
        Ok(())
    })
    .context("failed to serve MCP")?;
    Ok(())
}

/// Handle one inbound line; `None` when it warrants no reply (a notification).
fn handle_line(surface: &dyn ControlSurface, name: &str, line: &str) -> Option<Value> {
    let message: Value = match serde_json::from_str(line) {
        Ok(message) => message,
        Err(error) => {
            return Some(error_reply(
                Value::Null,
                PARSE_ERROR,
                &format!("invalid JSON: {error}"),
            ));
        }
    };
    let Some(message) = message.as_object() else {
        return Some(error_reply(
            Value::Null,
            INVALID_REQUEST,
            "expected a JSON-RPC object",
        ));
    };
    let id = message.get("id").cloned();
    let method = message.get("method").and_then(Value::as_str).unwrap_or("");
    let params = message.get("params").cloned().unwrap_or(Value::Null);

    // A message without an id is a notification and must not be answered;
    // a response from the client (a message without a method) is ignored.
    let id = id?;
    if method.is_empty() {
        return None;
    }

    Some(match method {
        "initialize" => initialize_reply(surface, id, name, &params),
        "ping" => reply(id, json!({})),
        "tools/list" => {
            let tools: Vec<Value> = surface
                .tools()
                .into_iter()
                .map(|tool| {
                    json!({
                        "name": tool.name,
                        "description": tool.description,
                        "inputSchema": tool.input_schema,
                    })
                })
                .collect();
            reply(id, json!({ "tools": tools }))
        }
        "tools/call" => tool_call_reply(surface, id, &params),
        _ => error_reply(id, METHOD_NOT_FOUND, &format!("unknown method {method}")),
    })
}

fn initialize_reply(surface: &dyn ControlSurface, id: Value, name: &str, params: &Value) -> Value {
    let proposed = params
        .get("protocolVersion")
        .and_then(Value::as_str)
        .unwrap_or(PROTOCOL_REVISION);
    let revision = if KNOWN_REVISIONS.contains(&proposed) {
        proposed
    } else {
        PROTOCOL_REVISION
    };
    let mut result = json!({
        "protocolVersion": revision,
        "capabilities": { "tools": {} },
        "serverInfo": {
            "name": name,
            "version": env!("CARGO_PKG_VERSION"),
        },
    });
    // The protocol's own home for "here is what this server is for and what
    // you must not do with it": clients hand it to the model before the first
    // tool call, where per-tool descriptions arrive too late to set policy.
    if let Some(instructions) = surface.instructions().filter(|text| !text.is_empty()) {
        result["instructions"] = Value::String(instructions);
    }
    reply(id, result)
}

fn tool_call_reply(surface: &dyn ControlSurface, id: Value, params: &Value) -> Value {
    let Some(tool) = params.get("name").and_then(Value::as_str) else {
        return error_reply(id, INVALID_PARAMS, "tools/call requires a tool name");
    };
    if !surface.tools().iter().any(|spec| spec.name == tool) {
        return error_reply(id, INVALID_PARAMS, &format!("unknown tool {tool}"));
    }
    let arguments = params.get("arguments").cloned().unwrap_or(json!({}));
    // A tool that could not do its work reports through the result, not a
    // protocol error: the model reads the message and can correct its call.
    match surface.call(tool, &arguments) {
        Ok(text) => reply(id, json!({ "content": [{ "type": "text", "text": text }] })),
        Err(error) => reply(
            id,
            json!({
                "content": [{ "type": "text", "text": format!("{error:#}") }],
                "isError": true,
            }),
        ),
    }
}

fn reply(id: Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

fn error_reply(id: Value, code: i64, message: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": code, "message": message },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::control::ToolSpec;

    struct EchoSurface;

    impl ControlSurface for EchoSurface {
        fn tools(&self) -> Vec<ToolSpec> {
            vec![ToolSpec {
                name: "echo",
                description: "Echo the message argument".into(),
                input_schema: json!({
                    "type": "object",
                    "properties": { "message": { "type": "string" } },
                    "required": ["message"],
                }),
            }]
        }

        fn instructions(&self) -> Option<String> {
            Some("echo back what you are told".into())
        }

        fn call(&self, name: &str, arguments: &Value) -> Result<String> {
            assert_eq!(name, "echo");
            let message = arguments
                .get("message")
                .and_then(Value::as_str)
                .context("echo requires a message")?;
            Ok(format!("echo: {message}"))
        }
    }

    fn transcript(lines: &[&str]) -> Vec<Value> {
        let input = lines.join("\n");
        let mut output = Vec::new();
        serve(
            &EchoSurface,
            "test",
            std::io::BufReader::new(input.as_bytes()),
            &mut output,
        )
        .unwrap();
        String::from_utf8(output)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    /// Replies arrive in completion order (each request runs on its own thread),
    /// so look one up by its JSON-RPC `id` rather than by position.
    fn by_id(replies: &[Value], id: i64) -> &Value {
        replies
            .iter()
            .find(|r| r.get("id").and_then(Value::as_i64) == Some(id))
            .unwrap_or_else(|| panic!("no reply for id {id} in {replies:?}"))
    }

    #[test]
    fn initialize_negotiates_a_known_revision_and_advertises_tools() {
        let replies = transcript(&[
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"t","version":"0"}}}"#,
            r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#,
        ]);
        assert_eq!(replies.len(), 2, "the notification must not be answered");
        let init = by_id(&replies, 1);
        let list = by_id(&replies, 2);
        assert_eq!(init["result"]["protocolVersion"], "2025-03-26");
        assert_eq!(init["result"]["serverInfo"]["name"], "test");
        assert!(init["result"]["capabilities"]["tools"].is_object());
        // Scope guidance rides the handshake, where a client can put it in
        // front of the model before it picks its first tool.
        assert_eq!(
            init["result"]["instructions"],
            "echo back what you are told"
        );
        assert_eq!(list["result"]["tools"][0]["name"], "echo");
        assert!(list["result"]["tools"][0]["inputSchema"].is_object());
    }

    #[test]
    fn unknown_revisions_fall_back_to_the_servers_latest() {
        let replies = transcript(&[
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2099-01-01"}}"#,
        ]);
        assert_eq!(replies[0]["result"]["protocolVersion"], PROTOCOL_REVISION);
    }

    #[test]
    fn tool_calls_answer_inline_and_failures_stay_results() {
        let replies = transcript(&[
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"echo","arguments":{"message":"hi"}}}"#,
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"echo","arguments":{}}}"#,
            r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"missing"}}"#,
        ]);
        let ok = by_id(&replies, 1);
        let bad = by_id(&replies, 2);
        let unknown = by_id(&replies, 3);
        assert_eq!(ok["result"]["content"][0]["text"], "echo: hi");
        assert!(ok["result"]["isError"].is_null());
        assert_eq!(bad["result"]["isError"], true);
        assert!(
            bad["result"]["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("message")
        );
        assert_eq!(unknown["error"]["code"], INVALID_PARAMS);
    }

    #[test]
    fn protocol_errors_name_their_cause_and_keep_the_session_alive() {
        let replies = transcript(&[
            "this is not json",
            r#"{"jsonrpc":"2.0","id":4,"method":"resources/list"}"#,
            r#"{"jsonrpc":"2.0","id":5,"method":"ping"}"#,
        ]);
        let parse = replies
            .iter()
            .find(|r| r.get("id").is_some_and(Value::is_null))
            .expect("parse-error reply carries a null id");
        let unknown = by_id(&replies, 4);
        let ping = by_id(&replies, 5);
        assert_eq!(parse["error"]["code"], PARSE_ERROR);
        assert_eq!(parse["id"], Value::Null);
        assert_eq!(unknown["error"]["code"], METHOD_NOT_FOUND);
        assert!(ping["result"].is_object());
    }
}
