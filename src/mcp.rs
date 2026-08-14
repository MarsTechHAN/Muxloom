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
pub fn serve(
    surface: &mut dyn ControlSurface,
    name: &str,
    reader: impl BufRead,
    mut writer: impl Write,
) -> Result<()> {
    for line in reader.lines() {
        let line = line.context("failed to read MCP transport")?;
        if line.trim().is_empty() {
            continue;
        }
        let Some(reply) = handle_line(surface, name, &line) else {
            continue;
        };
        let mut bytes = serde_json::to_vec(&reply).context("failed to encode MCP reply")?;
        bytes.push(b'\n');
        writer
            .write_all(&bytes)
            .and_then(|()| writer.flush())
            .context("failed to write MCP reply")?;
    }
    Ok(())
}

/// Handle one inbound line; `None` when it warrants no reply (a notification).
fn handle_line(surface: &mut dyn ControlSurface, name: &str, line: &str) -> Option<Value> {
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
        "initialize" => initialize_reply(id, name, &params),
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

fn initialize_reply(id: Value, name: &str, params: &Value) -> Value {
    let proposed = params
        .get("protocolVersion")
        .and_then(Value::as_str)
        .unwrap_or(PROTOCOL_REVISION);
    let revision = if KNOWN_REVISIONS.contains(&proposed) {
        proposed
    } else {
        PROTOCOL_REVISION
    };
    reply(
        id,
        json!({
            "protocolVersion": revision,
            "capabilities": { "tools": {} },
            "serverInfo": {
                "name": name,
                "version": env!("CARGO_PKG_VERSION"),
            },
        }),
    )
}

fn tool_call_reply(surface: &mut dyn ControlSurface, id: Value, params: &Value) -> Value {
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

        fn call(&mut self, name: &str, arguments: &Value) -> Result<String> {
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
        serve(&mut EchoSurface, "test", input.as_bytes(), &mut output).unwrap();
        String::from_utf8(output)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    #[test]
    fn initialize_negotiates_a_known_revision_and_advertises_tools() {
        let replies = transcript(&[
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"t","version":"0"}}}"#,
            r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#,
        ]);
        assert_eq!(replies.len(), 2, "the notification must not be answered");
        assert_eq!(replies[0]["result"]["protocolVersion"], "2025-03-26");
        assert_eq!(replies[0]["result"]["serverInfo"]["name"], "test");
        assert!(replies[0]["result"]["capabilities"]["tools"].is_object());
        assert_eq!(replies[1]["result"]["tools"][0]["name"], "echo");
        assert!(replies[1]["result"]["tools"][0]["inputSchema"].is_object());
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
        assert_eq!(replies[0]["result"]["content"][0]["text"], "echo: hi");
        assert!(replies[0]["result"]["isError"].is_null());
        assert_eq!(replies[1]["result"]["isError"], true);
        assert!(
            replies[1]["result"]["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("message")
        );
        assert_eq!(replies[2]["error"]["code"], INVALID_PARAMS);
    }

    #[test]
    fn protocol_errors_name_their_cause_and_keep_the_session_alive() {
        let replies = transcript(&[
            "this is not json",
            r#"{"jsonrpc":"2.0","id":4,"method":"resources/list"}"#,
            r#"{"jsonrpc":"2.0","id":5,"method":"ping"}"#,
        ]);
        assert_eq!(replies[0]["error"]["code"], PARSE_ERROR);
        assert_eq!(replies[0]["id"], Value::Null);
        assert_eq!(replies[1]["error"]["code"], METHOD_NOT_FOUND);
        assert!(replies[2]["result"].is_object());
    }
}
