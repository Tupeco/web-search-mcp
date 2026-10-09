//! URL allowlist patterns and the per-request policy built from them.
//!
//! Pattern syntax:
//!
//! ```text
//! *                                   any http or https URL
//! <scheme>://<host>[:<port>]<path>[?<query>]
//!   scheme  http, https or * (both)
//!   host    exact host, "*.example.com" (any subdomain, not the apex) or "*" (any host)
//!   port    number or "*"; omitted means the scheme's default port
//!   path    glob where "*" matches any characters, including "/"; omitted means "/"
//!   query   optional glob; when the pattern has no "?", any query string is allowed
//! ```
//!
//! Patterns and request URLs are both normalized with the WHATWG URL parser, so
//! "..", "%2e%2e", backslashes, IPv4 shorthands and IDNs can't sneak past a pattern.

use std::fmt;
use std::sync::Arc;
use url::Url;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Method {
    Get,
    Post,
    Put,
    Patch,
    Delete,
}

impl Method {
    pub const ALL: [Method; 5] = [Method::Get, Method::Post, Method::Put, Method::Patch, Method::Delete];

    pub fn as_str(self) -> &'static str {
        match self {
            Method::Get => "GET",
            Method::Post => "POST",
            Method::Put => "PUT",
            Method::Patch => "PATCH",
            Method::Delete => "DELETE",
        }
    }

    pub fn parse(name: &str) -> Option<Method> {
        Method::ALL.into_iter().find(|m| m.as_str().eq_ignore_ascii_case(name))
    }

    fn index(self) -> usize {
        self as usize
    }
}

impl fmt::Display for Method {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A `*`-only glob. `*` matches any sequence of characters.
#[derive(Clone, Debug)]
struct Glob {
    parts: Vec<String>,
}

impl Glob {
    fn new(glob: &str) -> Glob {
        Glob { parts: glob.split('*').map(str::to_owned).collect() }
    }

    fn matches(&self, text: &str) -> bool {
        let [first, middle @ .., last] = self.parts.as_slice() else {
            return text == self.parts[0];
        };
        if text.len() < first.len() + last.len() || !text.starts_with(first.as_str()) || !text.ends_with(last.as_str())
        {
            return false;
        }
        let mut rest = &text[first.len()..text.len() - last.len()];
        for part in middle {
            match rest.find(part.as_str()) {
                Some(i) => rest = &rest[i + part.len()..],
                None => return false,
            }
        }
        true
    }
}

#[derive(Clone, Debug)]
enum Scheme {
    Http,
    Https,
    Either,
}

#[derive(Clone, Debug)]
enum Host {
    Any,
    Exact(String),
    /// Stored with its leading dot: ".example.com".
    Subdomain(String),
}

#[derive(Clone, Debug)]
enum Port {
    Default,
    Any,
    Exact(u16),
}

#[derive(Clone, Debug)]
enum Kind {
    AnyUrl,
    Url { scheme: Scheme, host: Host, port: Port, path: Glob, query: Option<Glob> },
}

#[derive(Clone, Debug)]
pub struct Pattern {
    source: String,
    kind: Kind,
}

fn is_web(url: &Url) -> bool {
    matches!(url.scheme(), "http" | "https")
}

impl Pattern {
    pub fn compile(source: &str) -> Result<Pattern, String> {
        let text = source.trim();
        if text.is_empty() {
            return Err("URL pattern must be a non-empty string".into());
        }
        if text == "*" {
            return Ok(Pattern { source: text.into(), kind: Kind::AnyUrl });
        }
        let invalid = || {
            format!("invalid URL pattern \"{text}\" (expected scheme://host[:port]/path with scheme http, https or *)")
        };

        let (scheme, rest) = text.split_once("://").ok_or_else(invalid)?;
        let scheme = match scheme.to_ascii_lowercase().as_str() {
            "http" => Scheme::Http,
            "https" => Scheme::Https,
            "*" => Scheme::Either,
            _ => return Err(invalid()),
        };
        let (authority, path_query) = rest.split_at(rest.find(['/', '?', '#']).unwrap_or(rest.len()));
        if path_query.contains('#') {
            return Err(format!("URL pattern \"{text}\" must not contain a fragment"));
        }
        if authority.contains('@') {
            return Err(format!("URL pattern \"{text}\" must not contain credentials"));
        }

        let (host_raw, port_raw) = if authority.starts_with('[') {
            let close = authority.find(']').ok_or_else(invalid)?;
            authority.split_at(close + 1)
        } else {
            authority.split_at(authority.rfind(':').unwrap_or(authority.len()))
        };
        let port = match port_raw {
            "" => Port::Default,
            ":*" => Port::Any,
            p => match p.strip_prefix(':').and_then(|n| n.parse::<u16>().ok()) {
                Some(n) if n > 0 => Port::Exact(n),
                _ => return Err(format!("URL pattern \"{text}\" has an invalid port")),
            },
        };

        if host_raw.is_empty() {
            return Err(format!("URL pattern \"{text}\" has no host"));
        }
        let host = if host_raw == "*" {
            Host::Any
        } else {
            let (wildcard, literal) = match host_raw.strip_prefix("*.") {
                Some(rest) => (true, rest),
                None => (false, host_raw),
            };
            if literal.contains('*') {
                return Err(format!(
                    "URL pattern \"{text}\": \"*\" is only allowed as the whole host or as a leading \"*.\" label"
                ));
            }
            let normalized = Url::parse(&format!("https://{literal}/"))
                .ok()
                .and_then(|u| u.host_str().map(str::to_owned))
                .ok_or_else(|| format!("URL pattern \"{text}\" has an invalid host \"{literal}\""))?;
            if wildcard { Host::Subdomain(format!(".{normalized}")) } else { Host::Exact(normalized) }
        };

        // Prefixing the origin keeps "//foo" from being read as an authority.
        let parsed = Url::parse(&format!("https://placeholder.invalid{path_query}")).map_err(|_| invalid())?;
        let path = Glob::new(parsed.path());
        let query = path_query.contains('?').then(|| Glob::new(parsed.query().unwrap_or("")));

        Ok(Pattern { source: text.into(), kind: Kind::Url { scheme, host, port, path, query } })
    }

    pub fn source(&self) -> &str {
        &self.source
    }

    pub fn matches(&self, url: &Url) -> bool {
        match &self.kind {
            Kind::AnyUrl => is_web(url),
            Kind::Url { scheme, host, port, path, query } => {
                let scheme_ok = match scheme {
                    Scheme::Http => url.scheme() == "http",
                    Scheme::Https => url.scheme() == "https",
                    Scheme::Either => is_web(url),
                };
                let url_host = url.host_str().unwrap_or("");
                let host_ok = match host {
                    Host::Any => true,
                    Host::Exact(h) => url_host == h,
                    Host::Subdomain(suffix) => url_host.len() > suffix.len() && url_host.ends_with(suffix.as_str()),
                };
                let port_ok = match port {
                    Port::Default => url.port().is_none(),
                    Port::Any => true,
                    Port::Exact(p) => url.port_or_known_default() == Some(*p),
                };
                scheme_ok
                    && host_ok
                    && port_ok
                    && path.matches(url.path())
                    && query.as_ref().is_none_or(|q| q.matches(url.query().unwrap_or("")))
            }
        }
    }
}

/// Parses and normalizes a URL the model asked for, rejecting forms that the
/// target server might interpret differently from how the pattern matched them.
pub fn parse_target_url(input: &str) -> Result<Url, String> {
    let mut url = Url::parse(input).map_err(|e| format!("invalid URL \"{input}\": {e}"))?;
    if !is_web(&url) {
        return Err(format!("only http and https URLs are supported, got {}:", url.scheme()));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err("URLs with embedded credentials are not allowed; send an Authorization header instead".into());
    }
    let path = url.path().to_ascii_lowercase();
    if ["%2f", "%5c", "%00"].iter().any(|s| path.contains(s)) {
        return Err("encoded \"/\", \"\\\" or NUL characters in the URL path are not allowed".into());
    }
    // Some servers (e.g. Tomcat) treat "..;" as "..".
    if path.split('/').any(|seg| {
        let seg = seg.replace("%2e", ".");
        seg.starts_with(".;") || seg.starts_with("..;")
    }) {
        return Err("dot path segments with parameters (\"..;\") are not allowed".into());
    }
    url.set_fragment(None);
    Ok(url)
}

/// URL patterns for each HTTP method.
#[derive(Clone, Debug, Default)]
pub struct RuleSet {
    rules: [Vec<Pattern>; 5],
}

impl RuleSet {
    /// `entries` are (method name, patterns) pairs; `context` prefixes error messages.
    pub fn compile<'a>(
        entries: impl IntoIterator<Item = (&'a str, &'a [String])>,
        context: &str,
    ) -> Result<RuleSet, String> {
        let mut set = RuleSet::default();
        for (name, patterns) in entries {
            let method = Method::parse(name).ok_or_else(|| {
                format!("{context}: unknown HTTP method \"{name}\" (supported: GET, POST, PUT, PATCH, DELETE)")
            })?;
            for pattern in patterns {
                set.rules[method.index()]
                    .push(Pattern::compile(pattern).map_err(|e| format!("{context}.{name}: {e}"))?);
            }
        }
        Ok(set)
    }

    pub fn get(&self, method: Method) -> &[Pattern] {
        &self.rules[method.index()]
    }
}

/// Client-side allowlist from `X-Allow-<Method>` headers or `allow_<method>`
/// parameters on the MCP endpoint URL. Returns `None` when the client sent none.
/// When present, it is the client's complete list: methods it doesn't mention are denied.
pub fn parse_client_rules(headers: &[(String, String)], query: &[(String, String)]) -> Result<Option<RuleSet>, String> {
    let mut found = false;
    let mut lists: Vec<(&str, Vec<String>)> = Vec::new();
    for method in Method::ALL {
        let header_name = format!("x-allow-{}", method.as_str().to_ascii_lowercase());
        let param_name = format!("allow_{}", method.as_str().to_ascii_lowercase());
        let mut patterns = Vec::new();
        for (_, value) in headers.iter().filter(|(n, _)| n.eq_ignore_ascii_case(&header_name)) {
            found = true;
            // Whitespace-separated; a comma directly before whitespace is a separator too.
            patterns.extend(
                value
                    .split_whitespace()
                    .map(|p| p.strip_suffix(',').unwrap_or(p))
                    .filter(|p| !p.is_empty())
                    .map(str::to_owned),
            );
        }
        for (_, value) in query.iter().filter(|(n, _)| *n == param_name) {
            found = true;
            if !value.trim().is_empty() {
                patterns.push(value.trim().to_owned());
            }
        }
        lists.push((method.as_str(), patterns));
    }
    if !found {
        return Ok(None);
    }
    RuleSet::compile(lists.iter().map(|(m, p)| (*m, p.as_slice())), "client allowlist").map(Some)
}

/// A request must match the server rules for its token AND, when the client sent
/// its own allowlist, the client rules too. The client can only narrow access.
pub struct Policy {
    server: Arc<RuleSet>,
    client: Option<RuleSet>,
}

impl Policy {
    pub fn new(server: Arc<RuleSet>, client: Option<RuleSet>) -> Policy {
        Policy { server, client }
    }

    pub fn allows(&self, method: Method) -> bool {
        !self.server.get(method).is_empty() && self.client.as_ref().is_none_or(|c| !c.get(method).is_empty())
    }

    pub fn methods(&self) -> Vec<Method> {
        Method::ALL.into_iter().filter(|m| self.allows(*m)).collect()
    }

    pub fn check(&self, method: Method, url: &Url) -> Result<(), String> {
        if !self.allows(method) {
            return Err(format!("{method} requests are not enabled for this client"));
        }
        let server_ok = self.server.get(method).iter().any(|p| p.matches(url));
        let client_ok = self.client.as_ref().is_none_or(|c| c.get(method).iter().any(|p| p.matches(url)));
        if server_ok && client_ok {
            Ok(())
        } else {
            Err(format!("{method} {url} is not allowed. {}", self.describe(method)))
        }
    }

    pub fn describe(&self, method: Method) -> String {
        let list =
            |patterns: &[Pattern]| patterns.iter().map(|p| format!("- {}", p.source)).collect::<Vec<_>>().join("\n");
        let server = self.server.get(method);
        let header = "Allowed URL patterns (\"*\" matches any characters):";
        match &self.client {
            None => format!("{header}\n{}", list(server)),
            Some(client) if server.iter().any(|p| p.source == "*") => format!("{header}\n{}", list(client.get(method))),
            Some(client) => format!(
                "The URL must match one of these patterns (\"*\" matches any characters):\n{}\nand also one of these:\n{}",
                list(client.get(method)),
                list(server)
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn matches(pattern: &str, url: &str) -> bool {
        Pattern::compile(pattern).unwrap().matches(&parse_target_url(url).unwrap())
    }

    fn rules(entries: &[(&str, &[&str])]) -> RuleSet {
        let owned: Vec<(&str, Vec<String>)> =
            entries.iter().map(|(m, ps)| (*m, ps.iter().map(|p| p.to_string()).collect())).collect();
        RuleSet::compile(owned.iter().map(|(m, p)| (*m, p.as_slice())), "allow").unwrap()
    }

    #[test]
    fn exact_patterns() {
        assert!(matches("https://example.com/page", "https://example.com/page"));
        assert!(matches("https://example.com/page", "https://EXAMPLE.com/page?x=1#frag"), "any query without ?");
        assert!(!matches("https://example.com/page", "https://example.com/page/"));
        assert!(!matches("https://example.com/page", "http://example.com/page"), "scheme must match");
        assert!(matches("https://example.com", "https://example.com/"));
        assert!(!matches("https://example.com", "https://example.com/other"), "bare origin means / only");
    }

    #[test]
    fn path_wildcards() {
        let p = "https://example.com/api/*";
        assert!(matches(p, "https://example.com/api/"));
        assert!(matches(p, "https://example.com/api/v1/items/3"));
        assert!(!matches(p, "https://example.com/api"));
        assert!(!matches(p, "https://example.com/apiv2/x"));
        assert!(matches("https://example.com/*/edit", "https://example.com/docs/1/edit"));
        assert!(!matches("https://example.com/*/edit", "https://example.com/edit"));
        assert!(matches("https://example.com/a*b*c", "https://example.com/abc"));
        assert!(!matches("https://example.com/a*b*c", "https://example.com/acb"));
    }

    #[test]
    fn dot_segments_cannot_escape_a_path_pattern() {
        let p = "https://example.com/public/*";
        assert!(!matches(p, "https://example.com/public/../admin"));
        assert!(!matches(p, "https://example.com/public/%2e%2e/admin"));
        assert!(!matches(p, "https://example.com/public/.%2E/admin"));
        assert!(!matches(p, "https://example.com/public\\..\\admin"));
        for bad in [
            "https://example.com/public/..%2Fadmin",
            "https://example.com/public/..%5cadmin",
            "https://example.com/public/..;/admin",
            "https://example.com/public/%2e%2e;x/admin",
        ] {
            assert!(parse_target_url(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn host_matching() {
        assert!(!matches("https://example.com/*", "https://example.com.evil.net/"));
        assert!(!matches("https://example.com/*", "https://evil.net/?https://example.com/"));
        assert!(!matches("https://example.com/*", "https://notexample.com/"));
        assert!(matches("https://*.example.com/*", "https://a.b.example.com/x"));
        assert!(!matches("https://*.example.com/*", "https://example.com/x"), "wildcard excludes the apex");
        assert!(!matches("https://*.example.com/*", "https://badexample.com/x"));
        assert!(matches("https://bücher.de/*", "https://xn--bcher-kva.de/x"), "IDN normalized");
        assert!(matches("http://127.0.0.1/*", "http://2130706433/x"), "IPv4 shorthand normalized");
        assert!(matches("http://[::1]/*", "http://[0:0::1]/x"), "IPv6 normalized");
        assert!(matches("https://*/*", "https://anything.example/x"));
    }

    #[test]
    fn ports() {
        assert!(matches("https://example.com/*", "https://example.com:443/x"));
        assert!(!matches("https://example.com/*", "https://example.com:8443/x"));
        assert!(matches("https://example.com:8443/*", "https://example.com:8443/x"));
        assert!(matches("https://example.com:*/*", "https://example.com:9999/x"));
        assert!(matches("https://example.com:443/*", "https://example.com/x"));
    }

    #[test]
    fn scheme_wildcard_and_star() {
        assert!(matches("*://example.com/*", "http://example.com/x"));
        assert!(matches("*://example.com/*", "https://example.com/x"));
        assert!(matches("*", "http://anything.example:8080/x?y"));
    }

    #[test]
    fn query_patterns() {
        let p = "https://example.com/search?q=*";
        assert!(matches(p, "https://example.com/search?q=cats"));
        assert!(!matches(p, "https://example.com/search?page=2"));
        assert!(!matches(p, "https://example.com/search"));
    }

    #[test]
    fn target_url_validation() {
        for bad in [
            "ftp://example.com/",
            "file:///etc/passwd",
            "/relative",
            "https://user:pw@example.com/",
            "https://example.com@evil.net/",
        ] {
            assert!(parse_target_url(bad).is_err(), "{bad}");
        }
        assert_eq!(parse_target_url("https://example.com/a#b").unwrap().as_str(), "https://example.com/a");
    }

    #[test]
    fn invalid_patterns() {
        for bad in [
            "",
            "example.com/*",
            "ftp://example.com/",
            "https://ex*ample.com/",
            "https://a.*.com/",
            "https://user@example.com/",
            "https://example.com:99999/",
            "https://example.com:0/",
            "https://example.com/#x",
            "https:///path",
        ] {
            assert!(Pattern::compile(bad).is_err(), "{bad:?}");
        }
        let bad_method = [("FETCH", &[][..])];
        assert!(RuleSet::compile(bad_method, "allow").unwrap_err().contains("unknown HTTP method"));
    }

    #[test]
    fn client_rules_narrow_server_rules() {
        let server = Arc::new(rules(&[("GET", &["https://example.com/*"]), ("POST", &["https://example.com/api/*"])]));
        let headers = vec![(
            "X-Allow-Get".to_string(),
            "https://example.com/page https://other.net/*,  https://example.com/b".to_string(),
        )];
        let client = parse_client_rules(&headers, &[]).unwrap();
        let policy = Policy::new(server, client);
        let url = |s: &str| parse_target_url(s).unwrap();

        assert_eq!(policy.methods(), vec![Method::Get], "POST dropped because the client didn't list it");
        assert!(policy.check(Method::Get, &url("https://example.com/page")).is_ok());
        assert!(policy.check(Method::Get, &url("https://example.com/b")).is_ok());
        assert!(policy.check(Method::Get, &url("https://example.com/other")).unwrap_err().contains("not allowed"));
        assert!(policy.check(Method::Get, &url("https://other.net/x")).is_err(), "client can't widen");
        assert!(policy.check(Method::Post, &url("https://example.com/api/x")).unwrap_err().contains("not enabled"));
    }

    #[test]
    fn client_rules_from_query() {
        let query = vec![
            ("allow_get".to_string(), "https://example.com/a".to_string()),
            ("allow_post".to_string(), "https://example.com/api/*".to_string()),
        ];
        let client = parse_client_rules(&[], &query).unwrap().unwrap();
        assert_eq!(client.get(Method::Get)[0].source(), "https://example.com/a");
        assert_eq!(client.get(Method::Post)[0].source(), "https://example.com/api/*");
        assert!(client.get(Method::Put).is_empty());
        assert!(parse_client_rules(&[], &[("other".into(), "x".into())]).unwrap().is_none());
        assert!(parse_client_rules(&[], &[("allow_get".into(), "not a url".into())]).is_err());
    }

    #[test]
    fn describe_lists_effective_patterns() {
        let client = parse_client_rules(&[("x-allow-get".into(), "https://example.com/*".into())], &[]).unwrap();
        let open = Policy::new(Arc::new(rules(&[("GET", &["*"])])), client.clone());
        assert_eq!(
            open.describe(Method::Get),
            "Allowed URL patterns (\"*\" matches any characters):\n- https://example.com/*"
        );
        let narrow = Policy::new(Arc::new(rules(&[("GET", &["https://example.com/a/*"])])), client);
        assert!(narrow.describe(Method::Get).contains("and also one of these"));
    }
}
