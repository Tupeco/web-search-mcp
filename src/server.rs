//! HTTP front end: stateless MCP Streamable HTTP transport (JSON responses
//! only), token authentication and per-request policy construction.

use crate::config::{Client, Config, hash_token};
use crate::fetch::Fetcher;
use crate::log::Logger;
use crate::mcp::{SUPPORTED_PROTOCOL_VERSIONS, handle_message, rpc_error};
use crate::policy::{Policy, parse_client_rules};
use crate::tools::ToolContext;
use axum::Router;
use axum::body::{Body, to_bytes};
use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde_json::{Value, json};
use std::sync::Arc;

struct App {
    config: Config,
    fetcher: Fetcher,
    log: Logger,
}

pub fn router(config: Config, log: Logger) -> Result<Router, String> {
    let fetcher = Fetcher::new(&config)?;
    let app = Arc::new(App { config, fetcher, log });
    Ok(Router::new().fallback(handle).with_state(app))
}

fn json_response(status: StatusCode, payload: &Value) -> Response {
    (status, [(header::CONTENT_TYPE, "application/json")], payload.to_string()).into_response()
}

fn error_response(status: StatusCode, code: i64, message: &str) -> Response {
    json_response(status, &rpc_error(Value::Null, code, message))
}

fn text_response(status: StatusCode, text: &'static str) -> Response {
    (status, [(header::CONTENT_TYPE, "text/plain; charset=utf-8")], text).into_response()
}

fn constant_time_eq(a: &[u8; 32], b: &[u8; 32]) -> bool {
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn authenticate<'a>(config: &'a Config, headers: &HeaderMap, path_token: Option<&str>) -> Option<&'a Client> {
    let bearer = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split_once(' '))
        .filter(|(scheme, _)| scheme.eq_ignore_ascii_case("bearer"))
        .map(|(_, token)| token.trim());
    let token = bearer.or(path_token)?;
    let digest = hash_token(token);
    let mut found = None;
    for client in &config.clients {
        if constant_time_eq(&digest, &client.token_hash) {
            found = Some(client);
        }
    }
    found
}

async fn handle(State(app): State<Arc<App>>, request: Request) -> Response {
    let config = &app.config;
    let (parts, body) = request.into_parts();
    let path = parts.uri.path();

    if path == "/healthz" && parts.method == Method::GET {
        return text_response(StatusCode::OK, "ok\n");
    }
    // "/mcp", or "/mcp/<token>" when tokens in the URL are enabled.
    let path_token = match path.strip_prefix(config.path.as_str()) {
        Some("" | "/") => None,
        Some(rest) if config.allow_token_in_url => match rest.strip_prefix('/') {
            Some(token) if !token.is_empty() && !token.contains('/') => Some(token),
            _ => return text_response(StatusCode::NOT_FOUND, "not found\n"),
        },
        _ => return text_response(StatusCode::NOT_FOUND, "not found\n"),
    };

    // Browsers send Origin; MCP clients don't. Refusing unknown origins keeps
    // web pages from driving the server.
    if let Some(origin) = parts.headers.get(header::ORIGIN) {
        let allowed = origin.to_str().is_ok_and(|o| config.allowed_origins.iter().any(|a| a == o));
        if !allowed {
            let origin = String::from_utf8_lossy(origin.as_bytes());
            (app.log)(json!({"event": "rejected", "reason": "origin not in allowed_origins", "origin": origin}));
            return error_response(StatusCode::FORBIDDEN, -32000, "Origin not allowed");
        }
    }
    if parts.method != Method::POST {
        // No SSE stream and no sessions: the server is stateless.
        let mut response = error_response(StatusCode::METHOD_NOT_ALLOWED, -32000, "Method not allowed");
        response.headers_mut().insert(header::ALLOW, HeaderValue::from_static("POST"));
        return response;
    }

    let Some(client) = authenticate(config, &parts.headers, path_token) else {
        (app.log)(json!({"event": "rejected", "reason": "missing or unknown token"}));
        let mut response = error_response(StatusCode::UNAUTHORIZED, -32001, "Unauthorized");
        response.headers_mut().insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
        return response;
    };

    let header_pairs: Vec<(String, String)> = parts
        .headers
        .iter()
        .filter(|(name, _)| name.as_str().starts_with("x-allow-"))
        .map(|(name, value)| (name.to_string(), String::from_utf8_lossy(value.as_bytes()).into_owned()))
        .collect();
    let query_pairs: Vec<(String, String)> =
        url::form_urlencoded::parse(parts.uri.query().unwrap_or("").as_bytes()).into_owned().collect();
    let policy = match parse_client_rules(&header_pairs, &query_pairs) {
        Ok(client_rules) => Policy::new(client.rules.clone(), client_rules),
        Err(e) => return error_response(StatusCode::BAD_REQUEST, -32602, &format!("Invalid client allowlist: {e}")),
    };

    // JSON escaping can grow a request body, so allow headroom over the body limit.
    let max_message_bytes = (config.limits.max_request_body_bytes * 2 + 65_536).max(1 << 20);
    let bytes = match to_bytes(Body::new(body), max_message_bytes).await {
        Ok(bytes) => bytes,
        Err(_) => return error_response(StatusCode::PAYLOAD_TOO_LARGE, -32600, "Request too large"),
    };
    let Ok(payload) = serde_json::from_slice::<Value>(&bytes) else {
        return error_response(StatusCode::BAD_REQUEST, -32700, "Parse error");
    };

    let (messages, batch) = match &payload {
        Value::Array(items) => (items.iter().collect::<Vec<_>>(), true),
        single => (vec![single], false),
    };
    if messages.is_empty() {
        return error_response(StatusCode::BAD_REQUEST, -32600, "Invalid Request");
    }
    let initializing = messages.iter().any(|m| m.get("method").and_then(Value::as_str) == Some("initialize"));
    if let Some(version) = parts.headers.get("mcp-protocol-version") {
        let supported = version.to_str().is_ok_and(|v| SUPPORTED_PROTOCOL_VERSIONS.contains(&v));
        if !supported && !initializing {
            return error_response(StatusCode::BAD_REQUEST, -32600, "Unsupported MCP-Protocol-Version");
        }
    }

    // If the MCP client disconnects, this future is dropped, which cancels any
    // upstream request in progress.
    let ctx = ToolContext { config, client_name: &client.name, policy: &policy, fetcher: &app.fetcher, log: &app.log };
    let mut responses = Vec::new();
    for message in messages {
        if let Some(response) = handle_message(message, &ctx).await {
            responses.push(response);
        }
    }

    match (responses.len(), batch) {
        (0, _) => StatusCode::ACCEPTED.into_response(),
        (_, true) => json_response(StatusCode::OK, &Value::Array(responses)),
        (_, false) => json_response(StatusCode::OK, &responses[0]),
    }
}
