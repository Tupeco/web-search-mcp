//! Server configuration, loaded from a TOML file.

use crate::policy::RuleSet;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashSet};
use std::io::Read;
use std::sync::Arc;

#[derive(Debug, Clone)]
pub struct Limits {
    pub timeout_ms: u64,
    pub max_download_bytes: usize,
    pub max_request_body_bytes: usize,
    pub max_redirects: usize,
    pub default_output_chars: usize,
    pub max_output_chars: usize,
}

#[derive(Debug)]
pub struct Client {
    pub name: String,
    pub token_hash: [u8; 32],
    pub rules: Arc<RuleSet>,
}

#[derive(Debug)]
pub struct Config {
    pub host: String,
    pub port: u16,
    pub path: String,
    pub allow_token_in_url: bool,
    pub allow_private_networks: bool,
    pub allowed_origins: Vec<String>,
    pub user_agent: String,
    pub limits: Limits,
    pub clients: Vec<Client>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    #[serde(default)]
    listen: RawListen,
    path: Option<String>,
    #[serde(default)]
    allow_token_in_url: bool,
    #[serde(default)]
    allow_private_networks: bool,
    #[serde(default)]
    allowed_origins: Vec<String>,
    user_agent: Option<String>,
    #[serde(default)]
    limits: RawLimits,
    #[serde(default)]
    clients: Vec<RawClient>,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct RawListen {
    host: Option<String>,
    port: Option<u16>,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct RawLimits {
    timeout_ms: Option<u64>,
    max_download_bytes: Option<usize>,
    max_request_body_bytes: Option<usize>,
    max_redirects: Option<usize>,
    default_output_chars: Option<usize>,
    max_output_chars: Option<usize>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawClient {
    name: String,
    token_sha256: String,
    #[serde(default)]
    allow: BTreeMap<String, Vec<String>>,
}

fn positive<T: PartialOrd + Default + Copy>(value: Option<T>, default: T, name: &str) -> Result<T, String> {
    match value {
        Some(v) if v <= T::default() => Err(format!("limits.{name} must be greater than zero")),
        Some(v) => Ok(v),
        None => Ok(default),
    }
}

fn parse_hex32(text: &str) -> Option<[u8; 32]> {
    if text.len() != 64 || !text.is_ascii() {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&text[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(out)
}

pub fn to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn hash_token(token: &str) -> [u8; 32] {
    Sha256::digest(token.as_bytes()).into()
}

/// Generates a 256-bit client token, returned as (token, sha256 hex).
pub fn generate_token() -> std::io::Result<(String, String)> {
    let mut bytes = [0u8; 32];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    let token = to_hex(&bytes);
    let digest = to_hex(&hash_token(&token));
    Ok((token, digest))
}

impl Config {
    pub fn load(path: &str) -> Result<Config, String> {
        let text = std::fs::read_to_string(path).map_err(|e| format!("cannot read {path}: {e}"))?;
        Config::from_toml(&text)
    }

    pub fn from_toml(text: &str) -> Result<Config, String> {
        let raw: RawConfig = toml::from_str(text).map_err(|e| format!("invalid config: {e}"))?;
        let fail = |message: String| format!("invalid config: {message}");

        let path = raw.path.unwrap_or_else(|| "/mcp".into());
        if !path.starts_with('/') || path.len() < 2 || path.ends_with('/') {
            return Err(fail("path must start with \"/\" and not end with \"/\"".into()));
        }

        let l = raw.limits;
        let limits = Limits {
            timeout_ms: positive(l.timeout_ms, 30_000, "timeout_ms").map_err(fail)?,
            max_download_bytes: positive(l.max_download_bytes, 5_000_000, "max_download_bytes").map_err(fail)?,
            max_request_body_bytes: positive(l.max_request_body_bytes, 1_000_000, "max_request_body_bytes")
                .map_err(fail)?,
            max_redirects: l.max_redirects.unwrap_or(5),
            default_output_chars: positive(l.default_output_chars, 40_000, "default_output_chars").map_err(fail)?,
            max_output_chars: positive(l.max_output_chars, 200_000, "max_output_chars").map_err(fail)?,
        };
        if limits.default_output_chars > limits.max_output_chars {
            return Err(fail("limits.default_output_chars exceeds limits.max_output_chars".into()));
        }

        if raw.clients.is_empty() {
            return Err(fail("at least one [[clients]] entry is required".into()));
        }
        let mut names = HashSet::new();
        let mut hashes = HashSet::new();
        let mut clients = Vec::new();
        for (i, c) in raw.clients.into_iter().enumerate() {
            let context = format!("clients[{i}]");
            if c.name.is_empty() {
                return Err(fail(format!("{context}.name must not be empty")));
            }
            if !names.insert(c.name.clone()) {
                return Err(fail(format!("duplicate client name \"{}\"", c.name)));
            }
            let token_hash = parse_hex32(&c.token_sha256).ok_or_else(|| {
                fail(format!(
                    "{context}.token_sha256 must be a 64-character hex SHA-256 digest (run `web-search-mcp gen-token`)"
                ))
            })?;
            if !hashes.insert(token_hash) {
                return Err(fail(format!("{context} reuses another client's token")));
            }
            let rules =
                RuleSet::compile(c.allow.iter().map(|(m, p)| (m.as_str(), p.as_slice())), &format!("{context}.allow"))
                    .map_err(fail)?;
            clients.push(Client { name: c.name, token_hash, rules: Arc::new(rules) });
        }

        Ok(Config {
            host: raw.listen.host.unwrap_or_else(|| "127.0.0.1".into()),
            port: raw.listen.port.unwrap_or(8787),
            path,
            allow_token_in_url: raw.allow_token_in_url,
            allow_private_networks: raw.allow_private_networks,
            allowed_origins: raw.allowed_origins,
            user_agent: raw.user_agent.unwrap_or_else(|| concat!("web-search-mcp/", env!("CARGO_PKG_VERSION")).into()),
            limits,
            clients,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::Method;

    const HASH: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    fn client(extra: &str) -> String {
        format!("[[clients]]\nname = \"c\"\ntoken_sha256 = \"{HASH}\"\n{extra}\n[clients.allow]\nGET = [\"*\"]\n")
    }

    #[test]
    fn example_config_is_valid_once_filled_in() {
        let example = include_str!("../config.example.toml");
        assert!(Config::from_toml(example).unwrap_err().contains("token_sha256 must be"));
        let filled = example.replace("REPLACE_WITH_token_sha256_FROM_gen-token", HASH);
        let config = Config::from_toml(&filled).unwrap();
        assert_eq!(config.clients[0].rules.get(Method::Get).len(), 1);
        assert!(!config.allow_token_in_url);
        assert!(!config.allow_private_networks);
    }

    #[test]
    fn typos_and_bad_values_are_rejected() {
        let err = |text: String| Config::from_toml(&text).unwrap_err();
        assert!(err(format!("allow_private_network = true\n{}", client(""))).contains("unknown field"));
        assert!(err(format!("[limits]\ntimeout = 5\n{}", client(""))).contains("unknown field"));
        assert!(err(format!("[limits]\ntimeout_ms = 0\n{}", client(""))).contains("greater than zero"));
        assert!(err(client("").replace("\"*\"", "\"example.com\"")).contains("invalid URL pattern"));
        assert!(err(client("").replace("GET", "FETCH")).contains("unknown HTTP method"));
        assert!(err(format!("{}{}", client(""), client("").replace("\"c\"", "\"d\""))).contains("reuses another"));
        assert!(err(String::new()).contains("at least one"));
    }

    #[test]
    fn generated_tokens_hash_to_their_digest() {
        let (token, digest) = generate_token().unwrap();
        assert_eq!(token.len(), 64);
        assert_eq!(parse_hex32(&digest), Some(hash_token(&token)));
    }
}
