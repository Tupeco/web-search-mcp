//! End-to-end tests: a local target server, the MCP server in front of it, and
//! an HTTP client speaking MCP to the MCP server.

use axum::Router;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{any, get};
use serde_json::{Value, json};
use std::io::Write;
use std::sync::OnceLock;
use web_search_mcp::config::{Config, hash_token, to_hex};
use web_search_mcp::{log, server};

const TOKEN: &str = "full-access-token";
const GET_ONLY_TOKEN: &str = "get-only-token";

async fn serve(app: Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}")
}

async fn echo(method: axum::http::Method, headers: axum::http::HeaderMap, body: String) -> Response {
    let headers: serde_json::Map<String, Value> =
        headers.iter().map(|(k, v)| (k.to_string(), json!(v.to_str().unwrap_or("")))).collect();
    (
        StatusCode::CREATED,
        [(header::CONTENT_TYPE, "application/json"), (header::HeaderName::from_static("x-extra"), "yes")],
        json!({"method": method.as_str(), "headers": headers, "body": body}).to_string(),
    )
        .into_response()
}

fn gzip(text: &str) -> Vec<u8> {
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    encoder.write_all(text.as_bytes()).unwrap();
    encoder.finish().unwrap()
}

async fn start_target() -> String {
    let html = "<html><head><title>Test page</title></head><body><p>Hello <b>world</b></p></body></html>";
    serve(
        Router::new()
            .route("/page", get(move || async move { ([(header::CONTENT_TYPE, "text/html; charset=utf-8")], html) }))
            .route("/echo", any(echo))
            .route("/redirect-ok", get(|| async { Redirect::to("/page") }))
            .route("/redirect-out", get(|| async { Redirect::to("https://evil.example/steal") }))
            .route("/redirect-loop", get(|| async { Redirect::to("/redirect-loop") }))
            .route(
                "/gzip",
                get(|| async {
                    (
                        [(header::CONTENT_TYPE, "text/plain"), (header::CONTENT_ENCODING, "gzip")],
                        gzip("compressed hello"),
                    )
                }),
            )
            .route("/big", get(|| async { ([(header::CONTENT_TYPE, "text/plain")], "0123456789".repeat(1000)) }))
            .route(
                "/binary",
                get(|| async { ([(header::CONTENT_TYPE, "application/octet-stream")], vec![0u8, 1, 2, 3, 255]) }),
            )
            .route(
                "/latin1",
                get(|| async {
                    ([(header::CONTENT_TYPE, "text/plain; charset=iso-8859-1")], vec![0x50, 0xe4, 0xe4, 0x20, 0xf6])
                }),
            )
            .route(
                "/slow",
                get(|| async {
                    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
                    "late"
                }),
            ),
    )
    .await
}

fn config_toml(target: &str, extra: &str) -> String {
    format!(
        r#"
allow_private_networks = true
{extra}
[limits]
timeout_ms = 1000
max_download_bytes = 5000

[[clients]]
name = "full"
token_sha256 = "{full}"
[clients.allow]
GET = ["{target}/*"]
POST = ["{target}/echo"]
PUT = ["{target}/echo"]

[[clients]]
name = "get-only"
token_sha256 = "{get_only}"
[clients.allow]
GET = ["*"]
"#,
        full = to_hex(&hash_token(TOKEN)),
        get_only = to_hex(&hash_token(GET_ONLY_TOKEN)),
    )
}

async fn start_mcp(target: &str, extra: &str) -> String {
    let config = Config::from_toml(&config_toml(target, extra)).unwrap();
    format!("{}/mcp", serve(server::router(config, log::null_logger()).unwrap()).await)
}

struct Env {
    target: String,
    mcp: String,
}

/// One target and one default MCP server shared by all tests, on a runtime that outlives each test.
fn env() -> &'static Env {
    static ENV: OnceLock<Env> = OnceLock::new();
    ENV.get_or_init(|| {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let runtime = tokio::runtime::Runtime::new().unwrap();
            runtime.block_on(async {
                let target = start_target().await;
                let mcp = start_mcp(&target, "").await;
                tx.send(Env { target, mcp }).unwrap();
                std::future::pending::<()>().await;
            });
        });
        rx.recv().unwrap()
    })
}

struct Reply {
    status: StatusCode,
    headers: reqwest::header::HeaderMap,
    body: Value,
}

async fn post(url: &str, body: String, token: Option<&str>, headers: &[(&str, &str)]) -> Reply {
    let mut request = reqwest::Client::new()
        .post(url)
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream")
        .body(body);
    if let Some(token) = token {
        request = request.bearer_auth(token);
    }
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    let response = request.send().await.unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let text = response.text().await.unwrap();
    let body = if text.is_empty() { Value::Null } else { serde_json::from_str(&text).unwrap_or(Value::String(text)) };
    Reply { status, headers, body }
}

async fn rpc(url: &str, method: &str, params: Value) -> Reply {
    rpc_with(url, method, params, Some(TOKEN), &[]).await
}

async fn rpc_with(url: &str, method: &str, params: Value, token: Option<&str>, headers: &[(&str, &str)]) -> Reply {
    let body = json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params}).to_string();
    post(url, body, token, headers).await
}

async fn call(url: &str, name: &str, args: Value, token: &str, headers: &[(&str, &str)]) -> Value {
    rpc_with(url, "tools/call", json!({"name": name, "arguments": args}), Some(token), headers).await.body
}

async fn call_text(url: &str, name: &str, args: Value) -> String {
    let body = call(url, name, args, TOKEN, &[]).await;
    body["result"]["content"][0]["text"].as_str().unwrap_or_else(|| panic!("no text in {body}")).to_owned()
}

async fn tool_names(url: &str, token: &str, headers: &[(&str, &str)]) -> Vec<String> {
    let reply = rpc_with(url, "tools/list", json!({}), Some(token), headers).await;
    reply.body["result"]["tools"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap().to_owned()).collect()
}

#[tokio::test]
async fn rejects_missing_and_wrong_tokens() {
    let env = env();
    for token in [None, Some("wrong")] {
        let reply = rpc_with(&env.mcp, "tools/list", json!({}), token, &[]).await;
        assert_eq!(reply.status, StatusCode::UNAUTHORIZED);
        assert_eq!(reply.headers["www-authenticate"], "Bearer");
    }
}

#[tokio::test]
async fn initialize_negotiates_the_protocol_version() {
    let env = env();
    let known = rpc(&env.mcp, "initialize", json!({"protocolVersion": "2025-06-18", "capabilities": {}})).await;
    assert_eq!(known.body["result"]["protocolVersion"], "2025-06-18");
    assert_eq!(known.body["result"]["capabilities"], json!({"tools": {"listChanged": false}}));
    let unknown = rpc(&env.mcp, "initialize", json!({"protocolVersion": "1999-01-01"})).await;
    assert_eq!(unknown.body["result"]["protocolVersion"], "2025-11-25");
}

#[tokio::test]
async fn lists_one_tool_per_allowed_method() {
    let env = env();
    assert_eq!(tool_names(&env.mcp, TOKEN, &[]).await, ["http_get", "http_post", "http_put", "allowlist_info"]);
    assert_eq!(tool_names(&env.mcp, GET_ONLY_TOKEN, &[]).await, ["http_get", "allowlist_info"]);
    let reply = rpc(&env.mcp, "tools/list", json!({})).await;
    let tools = &reply.body["result"]["tools"];
    assert!(tools[0]["description"].as_str().unwrap().contains(&format!("- {}/*", env.target)));
    assert_eq!(tools[0]["annotations"]["readOnlyHint"], true);
    assert!(tools[0]["inputSchema"]["properties"].get("body").is_none());
    assert!(tools[1]["inputSchema"]["properties"].get("body").is_some());
}

#[tokio::test]
async fn client_headers_narrow_tools_and_urls() {
    let env = env();
    let only_page = format!("{}/page", env.target);
    let headers = [("x-allow-get", only_page.as_str())];
    assert_eq!(tool_names(&env.mcp, TOKEN, &headers).await, ["http_get", "allowlist_info"]);
    let ok = call(&env.mcp, "http_get", json!({"url": only_page}), TOKEN, &headers).await;
    assert!(ok["result"]["isError"].is_null(), "{ok}");
    let denied = call(&env.mcp, "http_get", json!({"url": format!("{}/big", env.target)}), TOKEN, &headers).await;
    assert_eq!(denied["result"]["isError"], true);
    assert!(denied["result"]["content"][0]["text"].as_str().unwrap().starts_with("Denied: GET "));
    let post = call(&env.mcp, "http_post", json!({"url": format!("{}/echo", env.target)}), TOKEN, &headers).await;
    assert_eq!(post["error"]["code"], -32602, "POST isn't exposed when the client lists only GET");
}

#[tokio::test]
async fn token_and_allowlist_in_the_url() {
    let env = env();
    let target = &env.target;
    let query = format!(
        "?allow_get={}",
        url::form_urlencoded::byte_serialize(format!("{target}/gzip").as_bytes()).collect::<String>()
    );

    // Disabled by default.
    let disabled = rpc_with(&format!("{}/{TOKEN}{query}", env.mcp), "tools/list", json!({}), None, &[]).await;
    assert_eq!(disabled.status, StatusCode::NOT_FOUND);

    let mcp = start_mcp(target, "allow_token_in_url = true").await;
    let url = format!("{mcp}/{TOKEN}{query}");
    let reply = rpc_with(&url, "tools/list", json!({}), None, &[]).await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.body["result"]["tools"].as_array().unwrap().len(), 2);
    let ok = rpc_with(
        &url,
        "tools/call",
        json!({"name": "http_get", "arguments": {"url": format!("{target}/gzip")}}),
        None,
        &[],
    )
    .await;
    assert!(ok.body["result"]["isError"].is_null());
    let denied = rpc_with(
        &url,
        "tools/call",
        json!({"name": "http_get", "arguments": {"url": format!("{target}/page")}}),
        None,
        &[],
    )
    .await;
    assert_eq!(denied.body["result"]["isError"], true);
    let wrong = rpc_with(&format!("{mcp}/wrong-token"), "tools/list", json!({}), None, &[]).await;
    assert_eq!(wrong.status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn header_token_with_allowlist_in_the_url() {
    // The setup a Claude connector uses: token in a request header, allowlist in the URL.
    let env = env();
    let target = &env.target;
    let encoded: String = url::form_urlencoded::byte_serialize(format!("{target}/gzip").as_bytes()).collect();
    let url = format!("{}?allow_get={encoded}", env.mcp);
    assert_eq!(tool_names(&url, TOKEN, &[]).await, ["http_get", "allowlist_info"]);
    let ok = call(&url, "http_get", json!({"url": format!("{target}/gzip")}), TOKEN, &[]).await;
    assert!(ok["result"]["isError"].is_null(), "{ok}");
    let denied = call(&url, "http_get", json!({"url": format!("{target}/page")}), TOKEN, &[]).await;
    assert_eq!(denied["result"]["isError"], true);
}

#[tokio::test]
async fn allowlist_info_reports_both_lists_and_their_sources() {
    let env = env();
    let target = &env.target;
    let server_only = call_text(&env.mcp, "allowlist_info", json!({})).await;
    let expected_get =
        format!("GET (http_get): enabled\n  Server allowlist:\n    - {target}/*\n  Connector allowlist: not set\n");
    assert!(server_only.starts_with("Allowlists for client \"full\".\n"), "{server_only}");
    assert!(server_only.contains("under [clients.allow] in the [[clients]] entry named \"full\""));
    assert!(server_only.contains(&expected_get), "{server_only}");
    assert!(
        server_only.contains("PATCH (http_patch): disabled (no patterns in config.toml)\n  Server allowlist: none")
    );
    assert!(server_only.contains("requests to private networks are allowed; timeout 1000 ms"));
    assert!(server_only.ends_with("This connection sent its token in the Authorization header."));

    let page = format!("{target}/page");
    let narrowed = call(&env.mcp, "allowlist_info", json!({}), TOKEN, &[("x-allow-get", page.as_str())]).await;
    let text = narrowed["result"]["content"][0]["text"].as_str().unwrap();
    assert!(text.contains(&format!("  Connector allowlist:\n    - {page}\n")), "{text}");
    assert!(text.contains("POST (http_post): disabled (the connector allowlist has no allow_post patterns)"));

    let with_args = call_text(&env.mcp, "allowlist_info", json!({"verbose": true})).await;
    assert_eq!(with_args, "Invalid arguments: allowlist_info takes no arguments");
}

#[tokio::test]
async fn invalid_client_allowlist_is_a_400() {
    let env = env();
    let reply = rpc_with(&env.mcp, "tools/list", json!({}), Some(TOKEN), &[("x-allow-get", "not-a-url")]).await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
    assert!(reply.body["error"]["message"].as_str().unwrap().contains("Invalid client allowlist"));
}

#[tokio::test]
async fn get_returns_status_headers_and_body() {
    let env = env();
    let url = format!("{}/page", env.target);
    assert_eq!(
        call_text(&env.mcp, "http_get", json!({"url": url})).await,
        "HTTP 200 OK\ncontent-type: text/html; charset=utf-8\n\n\
         <html><head><title>Test page</title></head><body><p>Hello <b>world</b></p></body></html>"
    );
    assert_eq!(
        call_text(&env.mcp, "http_get", json!({"url": url, "format": "text"})).await,
        "HTTP 200 OK\ncontent-type: text/html; charset=utf-8\n\nTitle: Test page\n\nHello world"
    );
}

#[tokio::test]
async fn post_sends_the_body_with_a_default_content_type() {
    let env = env();
    let url = format!("{}/echo", env.target);
    let text = call_text(
        &env.mcp,
        "http_post",
        json!({"url": url, "body": "{\"a\":1}", "headers": {"X-Custom": "v"}, "include_headers": true}),
    )
    .await;
    assert!(text.starts_with("HTTP 201 Created\n"), "{text}");
    assert!(text.contains("\nx-extra: yes\n"));
    let echoed: Value = serde_json::from_str(text.split_once("\n\n").unwrap().1).unwrap();
    assert_eq!(echoed["method"], "POST");
    assert_eq!(echoed["body"], "{\"a\":1}");
    assert_eq!(echoed["headers"]["content-type"], "application/json");
    assert_eq!(echoed["headers"]["x-custom"], "v");
    assert!(echoed["headers"]["user-agent"].as_str().unwrap().starts_with("web-search-mcp/"));

    let plain = call_text(&env.mcp, "http_put", json!({"url": url, "body": "hi"})).await;
    let echoed: Value = serde_json::from_str(plain.split_once("\n\n").unwrap().1).unwrap();
    assert_eq!(echoed["method"], "PUT");
    assert_eq!(echoed["headers"]["content-type"], "text/plain; charset=utf-8");
}

#[tokio::test]
async fn each_method_has_its_own_allowlist() {
    let env = env();
    let denied =
        call(&env.mcp, "http_post", json!({"url": format!("{}/page", env.target), "body": "x"}), TOKEN, &[]).await;
    assert_eq!(denied["result"]["isError"], true);
    assert!(denied["result"]["content"][0]["text"].as_str().unwrap().starts_with("Denied: POST "));
    let patch = call(&env.mcp, "http_patch", json!({"url": format!("{}/echo", env.target)}), TOKEN, &[]).await;
    assert_eq!(patch["error"]["code"], -32602);
}

#[tokio::test]
async fn redirects_are_followed_only_to_allowed_urls() {
    let env = env();
    let t = &env.target;
    let ok = call_text(&env.mcp, "http_get", json!({"url": format!("{t}/redirect-ok")})).await;
    assert!(ok.starts_with(&format!("HTTP 200 OK\nFinal URL: {t}/page (after 1 redirect)")), "{ok}");
    let out = call_text(&env.mcp, "http_get", json!({"url": format!("{t}/redirect-out")})).await;
    assert!(
        out.starts_with(&format!(
            "Denied: redirect from {t}/redirect-out was blocked: GET https://evil.example/steal is not allowed"
        )),
        "{out}"
    );
    let manual =
        call_text(&env.mcp, "http_get", json!({"url": format!("{t}/redirect-out"), "follow_redirects": false})).await;
    assert!(manual.starts_with("HTTP 303 See Other\nlocation: https://evil.example/steal"), "{manual}");
    let looping = call_text(&env.mcp, "http_get", json!({"url": format!("{t}/redirect-loop")})).await;
    assert_eq!(looping, "Request failed: too many redirects (limit 5)");
}

#[tokio::test]
async fn decompression_charsets_and_binary_bodies() {
    let env = env();
    let t = &env.target;
    assert!(
        call_text(&env.mcp, "http_get", json!({"url": format!("{t}/gzip")})).await.ends_with("\n\ncompressed hello")
    );
    assert!(call_text(&env.mcp, "http_get", json!({"url": format!("{t}/latin1")})).await.ends_with("\n\nPää ö"));
    assert!(
        call_text(&env.mcp, "http_get", json!({"url": format!("{t}/binary")}))
            .await
            .ends_with("[binary body of 5 bytes not shown]")
    );
}

#[tokio::test]
async fn long_bodies_are_paged_and_capped() {
    let env = env();
    let url = format!("{}/big", env.target);
    let first = call_text(&env.mcp, "http_get", json!({"url": url, "max_length": 15})).await;
    assert!(first.contains("\n\n012345678901234\n"), "{first}");
    assert!(first.contains("[download stopped at the server limit of 5000 bytes"));
    assert!(first.ends_with("[showing characters 0-15 of 5000; call again with offset=15 to read more]"));
    let next = call_text(&env.mcp, "http_get", json!({"url": url, "max_length": 15, "offset": 15})).await;
    assert!(next.contains("\n\n567890123456789\n"), "{next}");
}

#[tokio::test]
async fn argument_errors_are_tool_errors() {
    let env = env();
    let url = format!("{}/page", env.target);
    for args in [
        json!({}),
        json!({"url": url, "headers": {"Host": "evil"}}),
        json!({"url": url, "headers": {"X-A": "a\r\nInjected: 1"}}),
        json!({"url": url, "body": "x"}),
        json!({"url": url, "max_length": 0}),
        json!({"url": url, "format": "markdown"}),
        json!({"url": url, "constructor": 1}),
    ] {
        let text = call_text(&env.mcp, "http_get", args.clone()).await;
        assert!(text.starts_with("Invalid arguments: "), "{args}: {text}");
    }
    let text = call_text(&env.mcp, "http_get", json!({"url": "file:///etc/passwd"})).await;
    assert!(text.starts_with("Denied: only http and https"), "{text}");
}

#[tokio::test]
async fn timeouts() {
    let env = env();
    let text = call_text(&env.mcp, "http_get", json!({"url": format!("{}/slow", env.target)})).await;
    assert_eq!(text, "Request failed: timed out after 1000 ms");
}

#[tokio::test]
async fn private_networks_are_blocked_unless_enabled() {
    let env = env();
    let config =
        config_toml(&env.target, "").replace("allow_private_networks = true", "allow_private_networks = false");
    let mcp = format!(
        "{}/mcp",
        serve(server::router(Config::from_toml(&config).unwrap(), log::null_logger()).unwrap()).await
    );

    let literal = call_text(&mcp, "http_get", json!({"url": format!("{}/page", env.target)})).await;
    assert!(literal.starts_with("Denied: 127.0.0.1 is a non-public address"), "{literal}");
    let port = url::Url::parse(&env.target).unwrap().port().unwrap();
    let via_dns =
        call(&mcp, "http_get", json!({"url": format!("http://localhost:{port}/page")}), GET_ONLY_TOKEN, &[]).await;
    let text = via_dns["result"]["content"][0]["text"].as_str().unwrap();
    assert!(text.starts_with("Denied: localhost resolves only to non-public addresses"), "{text}");
}

#[tokio::test]
async fn transport_details() {
    let env = env();
    let notification =
        post(&env.mcp, json!({"jsonrpc": "2.0", "method": "notifications/initialized"}).to_string(), Some(TOKEN), &[])
            .await;
    assert_eq!(notification.status, StatusCode::ACCEPTED);
    assert!(notification.body.is_null());

    let get = reqwest::Client::new().get(&env.mcp).bearer_auth(TOKEN).send().await.unwrap();
    assert_eq!(get.status(), StatusCode::METHOD_NOT_ALLOWED);

    let origin = rpc_with(&env.mcp, "ping", json!({}), Some(TOKEN), &[("origin", "https://evil.example")]).await;
    assert_eq!(origin.status, StatusCode::FORBIDDEN);
    let bad_version =
        rpc_with(&env.mcp, "ping", json!({}), Some(TOKEN), &[("mcp-protocol-version", "1999-01-01")]).await;
    assert_eq!(bad_version.status, StatusCode::BAD_REQUEST);
    let good_version =
        rpc_with(&env.mcp, "ping", json!({}), Some(TOKEN), &[("mcp-protocol-version", "2025-06-18")]).await;
    assert_eq!(good_version.status, StatusCode::OK);

    let parse = post(&env.mcp, "{not json".into(), Some(TOKEN), &[]).await;
    assert_eq!(parse.status, StatusCode::BAD_REQUEST);
    assert_eq!(parse.body["error"]["code"], -32700);

    assert_eq!(rpc(&env.mcp, "resources/list", json!({})).await.body["error"]["code"], -32601);

    let batch = json!([
        {"jsonrpc": "2.0", "id": "a", "method": "ping"},
        {"jsonrpc": "2.0", "method": "notifications/initialized"}
    ]);
    let reply = post(&env.mcp, batch.to_string(), Some(TOKEN), &[]).await;
    assert_eq!(reply.body, json!([{"jsonrpc": "2.0", "id": "a", "result": {}}]));

    let base = env.mcp.trim_end_matches("/mcp");
    assert_eq!(reqwest::get(format!("{base}/healthz")).await.unwrap().status(), StatusCode::OK);
    assert_eq!(reqwest::get(format!("{base}/other")).await.unwrap().status(), StatusCode::NOT_FOUND);
}
