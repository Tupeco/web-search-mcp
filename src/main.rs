use std::process::ExitCode;
use web_search_mcp::config::{Config, generate_token};
use web_search_mcp::{log, server};

const USAGE: &str = "\
Usage:
  web-search-mcp [--config <path>]   run the server (default config: $CONFIG or config.toml)
  web-search-mcp gen-token           generate a client token and its token_sha256

Environment: CONFIG, HOST and PORT override the config file location and listen address.";

#[tokio::main]
async fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let config_path = match args.iter().map(String::as_str).collect::<Vec<_>>().as_slice() {
        [] => std::env::var("CONFIG").unwrap_or_else(|_| "config.toml".into()),
        ["--config", path] => path.to_string(),
        ["gen-token"] => {
            return match generate_token() {
                Ok((token, digest)) => {
                    println!("Token (give this to the MCP client and keep it secret):\n  {token}\n");
                    println!("token_sha256 (put this in config.toml):\n  {digest}");
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("cannot generate a token: {e}");
                    ExitCode::FAILURE
                }
            };
        }
        ["-h" | "--help"] => {
            println!("{USAGE}");
            return ExitCode::SUCCESS;
        }
        _ => {
            eprintln!("{USAGE}");
            return ExitCode::from(2);
        }
    };

    let config = match Config::load(&config_path) {
        Ok(config) => config,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::FAILURE;
        }
    };
    let host = std::env::var("HOST").unwrap_or_else(|_| config.host.clone());
    let port = match std::env::var("PORT").map(|p| p.parse::<u16>()) {
        Err(_) => config.port,
        Ok(Ok(port)) => port,
        Ok(Err(_)) => {
            eprintln!("PORT must be a port number");
            return ExitCode::FAILURE;
        }
    };

    let logger = log::stdout_logger();
    let startup = serde_json::json!({
        "event": "listening",
        "host": host,
        "port": port,
        "path": config.path,
        "clients": config.clients.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(),
        "allow_private_networks": config.allow_private_networks,
        "allow_token_in_url": config.allow_token_in_url,
    });
    let app = match server::router(config, logger.clone()) {
        Ok(app) => app,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::FAILURE;
        }
    };
    let listener = match tokio::net::TcpListener::bind((host.as_str(), port)).await {
        Ok(listener) => listener,
        Err(e) => {
            eprintln!("cannot listen on {host}:{port}: {e}");
            return ExitCode::FAILURE;
        }
    };
    logger(startup);

    if let Err(e) = axum::serve(listener, app).with_graceful_shutdown(shutdown_signal()).await {
        eprintln!("server error: {e}");
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}

async fn shutdown_signal() {
    let ctrl_c = tokio::signal::ctrl_c();
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).expect("SIGTERM handler");
    tokio::select! {
        _ = ctrl_c => {}
        _ = term.recv() => {}
    }
}
