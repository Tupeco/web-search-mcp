//! The subset of MCP this server needs: initialize, ping, tools/list and
//! tools/call over JSON-RPC 2.0. The server is stateless and never sends
//! requests or notifications of its own.

use crate::tools::{ToolContext, call_tool, list_tools};
use serde_json::{Value, json};

pub const SUPPORTED_PROTOCOL_VERSIONS: [&str; 4] = ["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];

const INSTRUCTIONS: &str = "Makes HTTP requests from the user's own server. Each HTTP method is a separate tool, \
    and only URLs matching the allowlist in that tool's description are permitted. \
    Use format=\"text\" to read HTML pages as plain text.";

pub fn rpc_error(id: Value, code: i64, message: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}

async fn dispatch(method: &str, params: &Value, ctx: &ToolContext<'_>) -> Result<Value, (i64, String)> {
    match method {
        "initialize" => {
            let requested = params.get("protocolVersion").and_then(Value::as_str).unwrap_or("");
            let version = SUPPORTED_PROTOCOL_VERSIONS
                .into_iter()
                .find(|v| *v == requested)
                .unwrap_or(SUPPORTED_PROTOCOL_VERSIONS[0]);
            Ok(json!({
                "protocolVersion": version,
                "capabilities": {"tools": {"listChanged": false}},
                "serverInfo": {"name": "web-search-mcp", "title": "Web requests", "version": env!("CARGO_PKG_VERSION")},
                "instructions": INSTRUCTIONS,
            }))
        }
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({"tools": list_tools(ctx.policy, &ctx.config.limits)})),
        "tools/call" => {
            let name = params.get("name").and_then(Value::as_str).ok_or((-32602, "Missing tool name".to_string()))?;
            let arguments = params.get("arguments").cloned().unwrap_or_else(|| json!({}));
            call_tool(name, &arguments, ctx).await.ok_or((-32602, format!("Unknown tool: {name}")))
        }
        _ => Err((-32601, format!("Method not found: {method}"))),
    }
}

/// Returns the JSON-RPC response for a message, or `None` for notifications and
/// client responses, which get none.
pub async fn handle_message(message: &Value, ctx: &ToolContext<'_>) -> Option<Value> {
    let Some(obj) = message.as_object().filter(|o| o.get("jsonrpc") == Some(&json!("2.0"))) else {
        return Some(rpc_error(Value::Null, -32600, "Invalid Request"));
    };
    let Some(method) = obj.get("method").and_then(Value::as_str) else {
        if obj.contains_key("result") || obj.contains_key("error") {
            return None;
        }
        return Some(rpc_error(obj.get("id").cloned().unwrap_or(Value::Null), -32600, "Invalid Request"));
    };
    let id = obj.get("id")?.clone(); // no id: a notification
    let params = obj.get("params").filter(|p| p.is_object()).cloned().unwrap_or_else(|| json!({}));
    Some(match dispatch(method, &params, ctx).await {
        Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
        Err((code, message)) => rpc_error(id, code, &message),
    })
}
