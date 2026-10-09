//! Performs one HTTP request for a tool call: checks every hop against the policy
//! and the network guard, and enforces the timeout and size limits.

use crate::config::Config;
use crate::netguard::{BlockedAddress, PublicOnlyResolver, is_public};
use crate::policy::{Method, Policy, parse_target_url};
use reqwest::StatusCode;
use reqwest::header::{AUTHORIZATION, COOKIE, HeaderMap, LOCATION, PROXY_AUTHORIZATION};
use std::error::Error as _;
use std::net::IpAddr;
use std::time::Duration;
use url::{Host, Url};

pub enum FetchError {
    /// Refused by the allowlist or the private-network guard.
    Denied(String),
    /// The request was attempted and failed.
    Failed(String),
}

pub struct FetchRequest {
    pub method: Method,
    pub url: Url,
    pub headers: HeaderMap,
    pub body: Option<String>,
    pub follow_redirects: bool,
}

pub struct FetchResponse {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub url: Url,
    pub redirects: usize,
    pub body: Vec<u8>,
    pub truncated: bool,
}

pub struct Fetcher {
    client: reqwest::Client,
    guard_private: bool,
    timeout: Duration,
    max_download_bytes: usize,
    max_redirects: usize,
}

fn reqwest_method(method: Method) -> reqwest::Method {
    match method {
        Method::Get => reqwest::Method::GET,
        Method::Post => reqwest::Method::POST,
        Method::Put => reqwest::Method::PUT,
        Method::Patch => reqwest::Method::PATCH,
        Method::Delete => reqwest::Method::DELETE,
    }
}

/// reqwest's Display hides the cause; include the whole source chain.
fn describe(err: &reqwest::Error) -> String {
    let mut message = err.to_string();
    let mut source = err.source();
    while let Some(cause) = source {
        message.push_str(": ");
        message.push_str(&cause.to_string());
        source = cause.source();
    }
    message
}

fn blocked_address(err: &reqwest::Error) -> Option<String> {
    let mut source = err.source();
    while let Some(cause) = source {
        if let Some(blocked) = cause.downcast_ref::<BlockedAddress>() {
            return Some(blocked.to_string());
        }
        source = cause.source();
    }
    None
}

impl Fetcher {
    pub fn new(config: &Config) -> Result<Fetcher, String> {
        let mut builder = reqwest::Client::builder()
            .user_agent(config.user_agent.clone())
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .no_proxy()
            .referer(false);
        if !config.allow_private_networks {
            builder = builder.dns_resolver(PublicOnlyResolver);
        }
        Ok(Fetcher {
            client: builder.build().map_err(|e| format!("cannot create HTTP client: {e}"))?,
            guard_private: !config.allow_private_networks,
            timeout: Duration::from_millis(config.limits.timeout_ms),
            max_download_bytes: config.limits.max_download_bytes,
            max_redirects: config.limits.max_redirects,
        })
    }

    pub async fn fetch(&self, policy: &Policy, request: FetchRequest) -> Result<FetchResponse, FetchError> {
        match tokio::time::timeout(self.timeout, self.fetch_inner(policy, request)).await {
            Ok(result) => result,
            Err(_) => Err(FetchError::Failed(format!("timed out after {} ms", self.timeout.as_millis()))),
        }
    }

    fn check_host(&self, url: &Url) -> Result<(), FetchError> {
        let ip = match url.host() {
            Some(Host::Ipv4(ip)) => IpAddr::V4(ip),
            Some(Host::Ipv6(ip)) => IpAddr::V6(ip),
            _ => return Ok(()), // domain names are checked by the resolver
        };
        if self.guard_private && !is_public(ip) {
            return Err(FetchError::Denied(format!(
                "{} is a non-public address; requests to private networks are disabled on this server",
                url.host_str().unwrap_or_default()
            )));
        }
        Ok(())
    }

    async fn fetch_inner(&self, policy: &Policy, request: FetchRequest) -> Result<FetchResponse, FetchError> {
        let FetchRequest { method, mut url, mut headers, body, follow_redirects } = request;
        let mut redirects = 0;
        let mut redirected_from: Option<Url> = None;
        loop {
            policy.check(method, &url).map_err(|reason| {
                FetchError::Denied(match &redirected_from {
                    Some(from) => format!("redirect from {from} was blocked: {reason}"),
                    None => reason,
                })
            })?;
            self.check_host(&url)?;

            let mut builder = self.client.request(reqwest_method(method), url.clone()).headers(headers.clone());
            if let Some(body) = &body {
                builder = builder.body(body.clone());
            }
            let mut response = builder.send().await.map_err(|e| match blocked_address(&e) {
                Some(reason) => FetchError::Denied(reason),
                None => FetchError::Failed(describe(&e)),
            })?;

            let status = response.status();
            if follow_redirects && status.is_redirection() && status != StatusCode::NOT_MODIFIED {
                if let Some(location) = response.headers().get(LOCATION) {
                    if redirects >= self.max_redirects {
                        return Err(FetchError::Failed(format!("too many redirects (limit {})", self.max_redirects)));
                    }
                    let location = String::from_utf8_lossy(location.as_bytes()).into_owned();
                    let next = url
                        .join(&location)
                        .map_err(|e| e.to_string())
                        .and_then(|joined| parse_target_url(joined.as_str()))
                        .map_err(|e| {
                            FetchError::Denied(format!("redirect from {url} to {location} was blocked: {e}"))
                        })?;
                    if next.origin() != url.origin() {
                        for name in [AUTHORIZATION, COOKIE, PROXY_AUTHORIZATION] {
                            headers.remove(name);
                        }
                    }
                    redirected_from = Some(std::mem::replace(&mut url, next));
                    redirects += 1;
                    continue;
                }
            }

            let mut collected = Vec::new();
            let mut truncated = false;
            while let Some(chunk) = response
                .chunk()
                .await
                .map_err(|e| FetchError::Failed(format!("error reading the response body: {}", describe(&e))))?
            {
                let room = self.max_download_bytes - collected.len();
                if chunk.len() > room {
                    collected.extend_from_slice(&chunk[..room]);
                    truncated = true;
                    break;
                }
                collected.extend_from_slice(&chunk);
            }
            return Ok(FetchResponse {
                status,
                headers: response.headers().clone(),
                url,
                redirects,
                body: collected,
                truncated,
            });
        }
    }
}
