//! Self-hosted MCP server that makes HTTP requests, with one tool per HTTP
//! method and per-method URL allowlists.

pub mod config;
pub mod fetch;
pub mod html_text;
pub mod log;
pub mod mcp;
pub mod netguard;
pub mod policy;
pub mod server;
pub mod tools;
