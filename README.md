# web-search-mcp

A small self-hosted MCP server, written in Rust, that makes HTTP requests for an AI agent such as a scheduled Claude task.

- **One tool per HTTP method:** `http_get`, `http_post`, `http_put`, `http_patch`, `http_delete`. Each tool can be allowed or blocked separately in Claude's connector settings.
- **URL allowlists per method:** set in the server config for each token. The connector URL can narrow them further.
- **No third party in the request path:** requests go from your server straight to the target site.
- **Private networks blocked by default:** the server refuses to reach loopback, LAN and cloud metadata addresses, so it can't be turned against your own host.

## Why

Claude's tool permissions work on whole tools: you can't allow a fetch tool for one URL only. With this server, the connector's tool permissions decide *which methods* the job can use, and the server decides *which URLs* each method can reach. The model can't change either one.

## How access is decided

A tool call goes through these checks in order:

1. **Connector permissions:** each tool can be allowed, made to need approval, or blocked in Claude's connector settings.
2. **Token:** the server only answers clients that present a token listed in `config.toml`.
3. **Server allowlist:** the patterns configured for that token and method.
4. **Client allowlist (optional):** patterns in the connector URL (`allow_get=…`) or in `X-Allow-*` headers. These can only narrow the server allowlist, never widen it.
5. **Network guard:** the resolved IP address must be public, unless `allow_private_networks` is set.

A method that's disabled by step 3 or 4 isn't listed as a tool at all. Every redirect target passes steps 3–5 again.

## Setup (Docker behind nginx)

1. **Build the image** (on the server, or anywhere with Docker):

   ```sh
   docker build -t web-search-mcp .
   ```

2. **Create a token.** The server stores only the token's SHA-256.

   ```sh
   docker run --rm web-search-mcp gen-token
   ```

3. **Write the config.** Copy `config.example.toml` to `deploy/config.toml`, paste the `token_sha256`, and set the allowlists:

   ```toml
   [[clients]]
   name = "scheduled-job"
   token_sha256 = "…"

   [clients.allow]
   GET = ["https://example.com/status/*"]
   ```

4. **Run the container.** The compose file publishes the port on `127.0.0.1` only, read-only, with all capabilities dropped:

   ```sh
   docker compose -f deploy/docker-compose.yml up -d --build
   ```

   Check that it answers (on the server itself):

   ```sh
   curl -s http://127.0.0.1:8787/mcp/$TOKEN -H 'Content-Type: application/json' \
     -d '{"jsonrpc":"2.0","id":1,"method":"tools/list"}'
   ```

5. **Add the nginx location.** Put [`deploy/nginx.conf`](deploy/nginx.conf) inside your existing TLS `server` block and reload nginx. The snippet does three things:
   - It allows only Anthropic's published outbound range, `160.79.104.0/21` (see [IP addresses](https://platform.claude.com/docs/en/api/ip-addresses)). To test from elsewhere, add your own IP temporarily.
   - It turns off the access log for this location, because the URL carries the token.
   - It proxies the requests to the container.

6. **Add the connector in Claude.** Add a custom connector with this URL:

   ```
   https://mcp.example.com/mcp/<TOKEN>
   ```

   Optionally, add a client-side allowlist to the URL, with each pattern percent-encoded:

   ```
   https://mcp.example.com/mcp/<TOKEN>?allow_get=https%3A%2F%2Fexample.com%2Fstatus%2F*
   ```

   In the connector's tool permissions, allow `http_get` without approval, so the unattended job can use it, and block anything else it doesn't need. Then attach the connector to the scheduled task.

The server logs one JSON line per event to stdout (`docker logs web-search-mcp`). For each request it logs the client name, method, URL without its query string, status, size and duration. It also logs denials with their reason, and rejected connections (an unknown token or a disallowed `Origin`). It never logs request or response bodies, headers, query strings or tokens.

## Configuration

`config.toml`. Set the path with `--config <path>` or `CONFIG`; `HOST` and `PORT` override `[listen]`. Unknown keys are rejected at startup, so a typo can't quietly disable a setting.

| Key | Default | Meaning |
| --- | --- | --- |
| `path` | `"/mcp"` | MCP endpoint path. |
| `allow_token_in_url` | `false` | Also accept the token as the last path segment (`/mcp/<token>`). Needed for Claude connectors, which take only a URL. |
| `allow_private_networks` | `false` | Allow requests to loopback, private, link-local, CGNAT/Tailscale and similar addresses. |
| `allowed_origins` | `[]` | Browser origins allowed to call the server. MCP clients normally don't send `Origin`. If connections fail and the log shows a rejected origin, add that origin here. |
| `user_agent` | `"web-search-mcp/<version>"` | Default `User-Agent` for outgoing requests. |
| `[listen] host`, `port` | `"127.0.0.1"`, `8787` | Listen address. The Docker image sets `HOST=0.0.0.0`. |
| `[limits] timeout_ms` | `30000` | Time limit for a whole request, including redirects and reading the body. |
| `[limits] max_download_bytes` | `5000000` | Maximum (decompressed) response bytes read. |
| `[limits] max_request_body_bytes` | `1000000` | Maximum request body the model can send. |
| `[limits] max_redirects` | `5` | Redirect limit (GET only). |
| `[limits] default_output_chars`, `max_output_chars` | `40000`, `200000` | Default and maximum `max_length` of a tool result. |
| `[[clients]]` | (required) | One entry per token: `name`, `token_sha256`, and an `[clients.allow]` table mapping `GET`, `POST`, `PUT`, `PATCH`, `DELETE` to lists of URL patterns. A method with no patterns is disabled. |

### URL patterns

| Pattern | Matches |
| --- | --- |
| `https://example.com/page` | exactly that path, with any query string |
| `https://example.com/api/*` | everything under `/api/` (`*` matches any characters, including `/`) |
| `https://example.com/*` | the whole site over https on port 443 |
| `https://*.example.com/*` | any subdomain, but not `example.com` itself |
| `https://example.com:8443/*` | an explicit port (`:*` means any port) |
| `*://example.com/*` | http or https |
| `https://example.com/search?q=*` | when the pattern contains `?`, the query string must match too |
| `*` | any http(s) URL |

A pattern with no path, such as `https://example.com`, means `/` only. Patterns and requested URLs are normalized the same way with the WHATWG URL parser before matching. That means `..`, `%2e%2e`, backslashes, upper-case hosts, IPv4 shorthands and IDNs can't be used to slip past a pattern.

A requested URL is rejected outright if it contains any of:

- credentials (`user:pw@`)
- an encoded `/`, `\` or NUL in the path
- `..;` path segments

### Client-side allowlists

The client can send its own allowlist. A request has to match both the server rules for the token **and** the client rules. Once the client sends any client-side rule, methods it doesn't mention are disabled.

- **URL parameters:** `allow_get`, `allow_post`, `allow_put`, `allow_patch`, `allow_delete`, with one percent-encoded pattern per parameter (repeat the parameter for more).
- **Headers**, for clients that support them: `X-Allow-Get`, `X-Allow-Post`, and so on, with patterns separated by spaces.

To manage allowlists entirely from the client side, give the token `GET = ["*"]` (or whichever methods it needs) on the server. Each tool's description shows the model the patterns that apply to it.

### Other clients

Clients that can send headers, such as the Claude Code CLI, can use the plain `/mcp` path with `Authorization: Bearer <token>` instead of putting the token in the URL. With such clients you can leave `allow_token_in_url` off.

```sh
claude mcp add --transport http web https://mcp.example.com/mcp \
  --header "Authorization: Bearer $TOKEN" --header "X-Allow-Get: https://example.com/status/*"
```

## Tools

Every tool takes these arguments:

- `url`: the absolute http(s) URL.
- `headers`: optional request headers. The server sets `Host`, `Content-Length`, `Connection`, `Transfer-Encoding` and `Accept-Encoding` itself.
- `format`: `raw` (the default) or `text`. `text` turns HTML into readable text with Markdown links.
- `max_length` and `offset`: read a long body in chunks. The result says which offset to request next.
- `include_headers`: return all response headers instead of only the key ones.

POST, PUT, PATCH and DELETE also take a `body` string. Without a `Content-Type` header, the body is sent as `application/json` if it parses as JSON, otherwise as `text/plain`.

`http_get` follows redirects. Each redirect target must pass the allowlist and the network guard, and `Authorization` and `Cookie` are dropped when a redirect leaves the original origin. The other methods return a 3xx response as-is and never retry a request.

The result starts with the status line and the key headers, followed by the body. Bodies are decompressed (gzip, deflate or br) and decoded using the response charset. Binary bodies are not returned. An HTTP error status is a normal result. Denials, invalid arguments and network failures are tool errors.

## Security notes

- **Tokens:** they are 256-bit random values, and only their SHA-256 is stored. Use a separate token for each job.
- **Network guard:** the private-network check runs on the resolved IP address, and the connection goes to that same address, so DNS rebinding can't get around it. IPv4-mapped, NAT64 and 6to4 forms of private IPv4 addresses are blocked too. Environment proxy settings are ignored.
- **Not a filter for content:** an allowed page can still contain text that tries to steer the model (prompt injection). Keep POST, PUT, PATCH and DELETE allowlists as narrow as the job allows.
- **Protocol:** the server implements the stateless subset of MCP Streamable HTTP (JSON responses, no sessions, no server-sent events). That's all a tools-only server needs.

## Development

```sh
cargo test     # unit tests, plus end-to-end tests against local target servers
cargo clippy --all-targets
```
