//! One MCP tool per HTTP method, so each can be permitted separately in the
//! client (e.g. "Always allow" for http_get only in a claude.ai connector).

use crate::config::{Config, Limits};
use crate::fetch::{FetchError, FetchRequest, FetchResponse, Fetcher};
use crate::html_text::html_to_text;
use crate::log::Logger;
use crate::policy::{Method, Policy, parse_target_url};
use reqwest::header::{CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue};
use serde_json::{Map, Value, json};
use std::time::Instant;
use url::Url;

pub struct ToolContext<'a> {
    pub config: &'a Config,
    pub client_name: &'a str,
    pub policy: &'a Policy,
    pub fetcher: &'a Fetcher,
    pub log: &'a Logger,
}

struct ToolSpec {
    name: &'static str,
    title: &'static str,
    summary: &'static str,
    read_only: bool,
    idempotent: bool,
}

fn spec(method: Method) -> ToolSpec {
    let (name, title, summary, read_only, idempotent) = match method {
        Method::Get => (
            "http_get",
            "HTTP GET",
            "Fetch a URL with an HTTP GET request. Redirects are followed only when the target URL is also allowed.",
            true,
            true,
        ),
        Method::Post => (
            "http_post",
            "HTTP POST",
            "Send an HTTP POST request. Redirects are not followed; a 3xx response is returned as-is.",
            false,
            false,
        ),
        Method::Put => (
            "http_put",
            "HTTP PUT",
            "Send an HTTP PUT request. Redirects are not followed; a 3xx response is returned as-is.",
            false,
            true,
        ),
        Method::Patch => (
            "http_patch",
            "HTTP PATCH",
            "Send an HTTP PATCH request. Redirects are not followed; a 3xx response is returned as-is.",
            false,
            false,
        ),
        Method::Delete => (
            "http_delete",
            "HTTP DELETE",
            "Send an HTTP DELETE request. Redirects are not followed; a 3xx response is returned as-is.",
            false,
            true,
        ),
    };
    ToolSpec { name, title, summary, read_only, idempotent }
}

fn method_for_tool(name: &str) -> Option<Method> {
    Method::ALL.into_iter().find(|m| spec(*m).name == name)
}

fn has_body(method: Method) -> bool {
    method != Method::Get
}

/// Headers the server sets itself; letting the model set them could break framing.
const MANAGED_HEADERS: [&str; 11] = [
    "host",
    "content-length",
    "transfer-encoding",
    "connection",
    "keep-alive",
    "upgrade",
    "te",
    "trailer",
    "proxy-connection",
    "accept-encoding",
    "expect",
];
const SUMMARY_HEADERS: [&str; 4] = ["content-type", "location", "retry-after", "www-authenticate"];

fn input_schema(method: Method, limits: &Limits) -> Value {
    let mut properties = Map::new();
    properties.insert(
        "url".into(),
        json!({"type": "string", "description": "Absolute http(s) URL. It must match the allowlist in the tool description."}),
    );
    properties.insert("headers".into(), json!({
        "type": "object",
        "additionalProperties": {"type": "string"},
        "description": "Extra request headers. Host, Content-Length, Connection, Transfer-Encoding and Accept-Encoding are set by the server."
    }));
    if has_body(method) {
        properties.insert("body".into(), json!({
            "type": "string",
            "description": "Request body. Without a Content-Type header it is sent as application/json if it is valid JSON, otherwise as text/plain."
        }));
    }
    properties.insert("format".into(), json!({
        "type": "string",
        "enum": ["raw", "text"],
        "default": "raw",
        "description": "\"raw\" returns the body as-is. \"text\" converts an HTML response to readable text with Markdown links; other content types are returned unchanged."
    }));
    properties.insert(
        "max_length".into(),
        json!({
            "type": "integer",
            "minimum": 1,
            "maximum": limits.max_output_chars,
            "default": limits.default_output_chars,
            "description": "Maximum number of characters of the response body to return."
        }),
    );
    properties.insert(
        "offset".into(),
        json!({
            "type": "integer",
            "minimum": 0,
            "default": 0,
            "description": "Character offset into the (converted) body, for reading a long response in chunks."
        }),
    );
    properties.insert(
        "include_headers".into(),
        json!({
            "type": "boolean",
            "default": false,
            "description": "Include all response headers instead of only the most relevant ones."
        }),
    );
    if method == Method::Get {
        properties.insert(
            "follow_redirects".into(),
            json!({
                "type": "boolean",
                "default": true,
                "description": "Follow redirects to allowed URLs."
            }),
        );
    }
    json!({"type": "object", "properties": properties, "required": ["url"], "additionalProperties": false})
}

pub fn list_tools(policy: &Policy, limits: &Limits) -> Vec<Value> {
    policy
        .methods()
        .into_iter()
        .map(|method| {
            let s = spec(method);
            json!({
                "name": s.name,
                "title": s.title,
                "description": format!("{}\n\n{}", s.summary, policy.describe(method)),
                "inputSchema": input_schema(method, limits),
                "annotations": {
                    "title": s.title,
                    "readOnlyHint": s.read_only,
                    "destructiveHint": !s.read_only,
                    "idempotentHint": s.idempotent,
                    "openWorldHint": true,
                },
            })
        })
        .collect()
}

struct Args {
    url: String,
    headers: HeaderMap,
    body: Option<String>,
    text_format: bool,
    max_length: usize,
    offset: usize,
    include_headers: bool,
    follow_redirects: bool,
}

fn validate(method: Method, args: &Value, limits: &Limits) -> Result<Args, String> {
    let Value::Object(raw) = args else { return Err("arguments must be an object".into()) };
    // Explicit nulls count as omitted.
    let args: Map<String, Value> =
        raw.iter().filter(|(_, v)| !v.is_null()).map(|(k, v)| (k.clone(), v.clone())).collect();
    let schema = input_schema(method, limits);
    let known = schema["properties"].as_object().expect("schema has properties");
    if let Some(unknown) = args.keys().find(|k| !known.contains_key(*k)) {
        return Err(format!("unknown argument \"{unknown}\""));
    }

    let url = match args.get("url") {
        Some(Value::String(s)) if !s.is_empty() => s.clone(),
        _ => return Err("\"url\" must be a non-empty string".into()),
    };

    let mut headers = HeaderMap::new();
    match args.get("headers") {
        None => {}
        Some(Value::Object(map)) => {
            for (name, value) in map {
                let header =
                    HeaderName::from_bytes(name.as_bytes()).map_err(|_| format!("invalid header name \"{name}\""))?;
                if MANAGED_HEADERS.contains(&header.as_str()) {
                    return Err(format!("header \"{name}\" is set by the server and cannot be overridden"));
                }
                let value = value
                    .as_str()
                    .and_then(|v| HeaderValue::from_bytes(v.as_bytes()).ok())
                    .ok_or_else(|| format!("header \"{name}\" must be a single-line string"))?;
                headers.append(header, value);
            }
        }
        Some(_) => return Err("\"headers\" must be an object".into()),
    }

    let body = match args.get("body") {
        None => None,
        Some(Value::String(s)) if s.len() > limits.max_request_body_bytes => {
            return Err(format!("\"body\" exceeds the server limit of {} bytes", limits.max_request_body_bytes));
        }
        Some(Value::String(s)) => Some(s.clone()),
        Some(_) => return Err("\"body\" must be a string".into()),
    };
    if body.is_some() && !headers.contains_key(CONTENT_TYPE) {
        let is_json = serde_json::from_str::<serde::de::IgnoredAny>(body.as_deref().unwrap_or("")).is_ok();
        let content_type = if is_json { "application/json" } else { "text/plain; charset=utf-8" };
        headers.insert(CONTENT_TYPE, HeaderValue::from_static(content_type));
    }

    let text_format = match args.get("format").map(Value::as_str) {
        None | Some(Some("raw")) => false,
        Some(Some("text")) => true,
        _ => return Err("\"format\" must be \"raw\" or \"text\"".into()),
    };
    let max_length = match args.get("max_length") {
        None => limits.default_output_chars,
        Some(v) => match v.as_u64() {
            Some(n) if n >= 1 && n <= limits.max_output_chars as u64 => n as usize,
            _ => return Err(format!("\"max_length\" must be an integer between 1 and {}", limits.max_output_chars)),
        },
    };
    let offset = match args.get("offset") {
        None => 0,
        Some(v) => v.as_u64().ok_or("\"offset\" must be a non-negative integer")? as usize,
    };
    let flag = |name: &str, default: bool| match args.get(name) {
        None => Ok(default),
        Some(Value::Bool(b)) => Ok(*b),
        Some(_) => Err(format!("\"{name}\" must be a boolean")),
    };
    let include_headers = flag("include_headers", false)?;
    let follow_redirects = method == Method::Get && flag("follow_redirects", true)?;

    Ok(Args { url, headers, body, text_format, max_length, offset, include_headers, follow_redirects })
}

fn media_type(content_type: Option<&str>) -> String {
    content_type.unwrap_or("").split(';').next().unwrap_or("").trim().to_ascii_lowercase()
}

fn is_textual(content_type: Option<&str>, body: &[u8]) -> bool {
    let media = media_type(content_type);
    if media.is_empty() {
        return !body[..body.len().min(1024)].contains(&0);
    }
    media.starts_with("text/")
        || media.ends_with("/json")
        || media.ends_with("+json")
        || media.ends_with("/xml")
        || media.ends_with("+xml")
        || [
            "application/javascript",
            "application/ecmascript",
            "application/x-javascript",
            "application/x-www-form-urlencoded",
            "application/x-ndjson",
            "application/yaml",
            "application/x-yaml",
            "application/toml",
            "application/graphql",
            "application/sql",
        ]
        .contains(&media.as_str())
}

fn is_html(content_type: Option<&str>, text: &str) -> bool {
    let media = media_type(content_type);
    if !media.is_empty() {
        return media == "text/html" || media == "application/xhtml+xml";
    }
    let start = text.trim_start();
    let head = start[..start.len().min(15)].to_ascii_lowercase();
    head.starts_with("<!doctype html") || head.starts_with("<html")
}

fn charset_label(content_type: Option<&str>, body: &[u8]) -> Option<String> {
    fn after_charset(text: &str) -> Option<String> {
        let lower = text.to_ascii_lowercase();
        let i = lower.find("charset")?;
        let rest = lower[i + 7..].trim_start().strip_prefix('=')?.trim_start();
        let label: String = rest
            .trim_start_matches(['"', '\''])
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | ':'))
            .collect();
        (!label.is_empty()).then_some(label)
    }
    if let Some(label) = content_type.and_then(after_charset) {
        return Some(label);
    }
    if media_type(content_type) == "text/html" {
        let head = String::from_utf8_lossy(&body[..body.len().min(4096)]);
        let lower = head.to_ascii_lowercase();
        let meta = lower.find("<meta")?;
        return after_charset(&head[meta..]);
    }
    None
}

fn decode_text(body: &[u8], content_type: Option<&str>) -> String {
    let encoding = charset_label(content_type, body)
        .and_then(|label| encoding_rs::Encoding::for_label(label.as_bytes()))
        .unwrap_or(encoding_rs::UTF_8);
    encoding.decode(body).0.into_owned()
}

fn header_text(value: &HeaderValue) -> String {
    String::from_utf8_lossy(value.as_bytes()).into_owned()
}

fn format_response(response: &FetchResponse, args: &Args, limits: &Limits) -> String {
    let mut lines = vec![
        format!("HTTP {} {}", response.status.as_u16(), response.status.canonical_reason().unwrap_or(""))
            .trim_end()
            .to_owned(),
    ];
    if response.redirects > 0 {
        let plural = if response.redirects == 1 { "" } else { "s" };
        lines.push(format!("Final URL: {} (after {} redirect{plural})", response.url, response.redirects));
    }
    if args.include_headers {
        for (name, value) in &response.headers {
            lines.push(format!("{name}: {}", header_text(value)));
        }
    } else {
        for name in SUMMARY_HEADERS {
            let values: Vec<String> = response.headers.get_all(name).iter().map(header_text).collect();
            if !values.is_empty() {
                lines.push(format!("{name}: {}", values.join(", ")));
            }
        }
    }

    let mut notes = Vec::new();
    let content_type = response.headers.get(CONTENT_TYPE).and_then(|v| v.to_str().ok());
    let mut text = String::new();
    if !response.body.is_empty() {
        if is_textual(content_type, &response.body) {
            text = decode_text(&response.body, content_type);
            if args.text_format && is_html(content_type, &text) {
                text = html_to_text(&text, &response.url);
            }
        } else {
            notes.push(format!("binary body of {} bytes not shown", response.body.len()));
        }
    }
    if response.truncated {
        notes.push(format!(
            "download stopped at the server limit of {} bytes, so the body is incomplete",
            limits.max_download_bytes
        ));
    }

    let total = text.chars().count();
    let start = args.offset.min(total);
    let end = args.offset.saturating_add(args.max_length).min(total);
    let shown: String = text.chars().skip(start).take(end - start).collect();
    if args.offset > 0 || end < total {
        let more = if end < total { format!("; call again with offset={end} to read more") } else { String::new() };
        notes.push(format!("showing characters {start}-{end} of {total}{more}"));
    }
    if response.body.is_empty() {
        notes.push("empty body".into());
    }

    lines.push(String::new());
    lines.push(shown);
    lines.extend(notes.into_iter().map(|n| format!("[{n}]")));
    lines.join("\n").trim_end().to_owned()
}

fn tool_error(message: String) -> Value {
    json!({"content": [{"type": "text", "text": message}], "isError": true})
}

fn log_url(url: &Url) -> String {
    // Query strings often carry API keys, so they stay out of the log.
    format!("{}{}", url.origin().ascii_serialization(), url.path())
}

/// Runs a tool. Returns `None` if the tool doesn't exist for this client.
pub async fn call_tool(name: &str, args: &Value, ctx: &ToolContext<'_>) -> Option<Value> {
    let method = method_for_tool(name).filter(|m| ctx.policy.allows(*m))?;
    let args = match validate(method, args, &ctx.config.limits) {
        Ok(args) => args,
        Err(message) => return Some(tool_error(format!("Invalid arguments: {message}"))),
    };

    let started = Instant::now();
    let url = match parse_target_url(&args.url) {
        Ok(url) => url,
        Err(reason) => {
            let shown: String = args.url.chars().take(200).collect();
            (ctx.log)(
                json!({"event": "denied", "client": ctx.client_name, "method": method.as_str(), "url": shown, "reason": reason}),
            );
            return Some(tool_error(format!("Denied: {reason}")));
        }
    };
    let request = FetchRequest {
        method,
        url: url.clone(),
        headers: args.headers.clone(),
        body: args.body.clone(),
        follow_redirects: args.follow_redirects,
    };
    let result = ctx.fetcher.fetch(ctx.policy, request).await;
    let ms = started.elapsed().as_millis() as u64;
    Some(match result {
        Ok(response) => {
            (ctx.log)(json!({
                "event": "request",
                "client": ctx.client_name,
                "method": method.as_str(),
                "url": log_url(&url),
                "final_url": (response.redirects > 0).then(|| log_url(&response.url)),
                "status": response.status.as_u16(),
                "bytes": response.body.len(),
                "ms": ms,
            }));
            json!({"content": [{"type": "text", "text": format_response(&response, &args, &ctx.config.limits)}]})
        }
        Err(FetchError::Denied(reason)) => {
            (ctx.log)(
                json!({"event": "denied", "client": ctx.client_name, "method": method.as_str(), "url": log_url(&url), "reason": reason}),
            );
            tool_error(format!("Denied: {reason}"))
        }
        Err(FetchError::Failed(error)) => {
            (ctx.log)(
                json!({"event": "failed", "client": ctx.client_name, "method": method.as_str(), "url": log_url(&url), "error": error, "ms": ms}),
            );
            tool_error(format!("Request failed: {error}"))
        }
    })
}
