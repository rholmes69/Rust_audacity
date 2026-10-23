//! Rust Audacity MCP server (stdio).
//!
//! Exposes the Rust Audacity audio engine to AI agents (Claude Desktop, Claude Code,
//! Cursor, …) over the Model Context Protocol using the stdio transport:
//! newline-delimited JSON-RPC 2.0 on stdin/stdout. Logs go to stderr.
//! The same server is also built into the desktop app over HTTP (Agents ▸ MCP Server).
//!
//! Usage:  rust_audacity_mcp [--root <dir>] [--demucs-command "<cmd>"] [--python <python>]
//!   --root            only allow reading/writing files inside this folder (recommended)
//!   --python          Python with faster-whisper installed, for transcribe_audio (default python/python3)
//!   --demucs-command  command used for AI stem separation (default: "python -m demucs" on
//!                     Windows, "demucs" elsewhere)

use std::{
    io::{BufRead, Write},
    sync::{Arc, Mutex},
};

use rust_audacity::mcp::{self, Config, Kind, Server};
use serde_json::Value;

/// Shared, line-oriented stdout writer (one JSON message per line).
#[derive(Clone)]
struct Out(Arc<Mutex<std::io::Stdout>>);

impl Out {
    fn send(&self, msg: &Value) {
        let mut s = serde_json::to_string(msg).unwrap_or_default();
        s.push('\n');
        let mut o = self.0.lock().unwrap();
        let _ = o.write_all(s.as_bytes());
        let _ = o.flush();
    }
}

fn parse_args() -> Result<Config, String> {
    let mut cfg = Config::default();
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--root" => {
                let dir = args.next().ok_or("--root needs a folder")?;
                let p = std::fs::canonicalize(&dir).map_err(|e| format!("--root {dir}: {e}"))?;
                cfg.root = Some(p);
            }
            "--demucs-command" => {
                cfg.demucs_command = args.next().ok_or("--demucs-command needs a value")?
            }
            "--python" => cfg.python = args.next().ok_or("--python needs a value")?,
            "-h" | "--help" => {
                eprintln!(
                    "rust_audacity_mcp [--root <dir>] [--demucs-command \"<cmd>\"] [--python <python>]\nMCP server over stdio; configure it in your MCP client."
                );
                std::process::exit(0);
            }
            "--version" => {
                println!("{}", env!("CARGO_PKG_VERSION"));
                std::process::exit(0);
            }
            other => return Err(format!("unknown argument {other}")),
        }
    }
    Ok(cfg)
}

fn main() {
    let cfg = match parse_args() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("rust_audacity_mcp: {e}");
            std::process::exit(2);
        }
    };
    eprintln!(
        "rust_audacity_mcp {} ready (root: {})",
        env!("CARGO_PKG_VERSION"),
        cfg.root
            .as_ref()
            .map_or("unrestricted".into(), |p| p.display().to_string())
    );
    let server = Server::with_log(cfg, |line| eprintln!("{line}"));
    let out = Out(Arc::new(Mutex::new(std::io::stdout())));
    let mut workers = Vec::new();

    for line in std::io::stdin().lock().lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        let msg: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(e) => {
                out.send(&mcp::error(
                    &Value::Null,
                    -32700,
                    &format!("parse error: {e}"),
                ));
                continue;
            }
        };
        let o = out.clone();
        let notify: mcp::Notify = Arc::new(move |n| o.send(&n));
        let is_call = msg.get("method").and_then(Value::as_str) == Some("tools/call");
        if is_call && matches!(mcp::kind(&msg), Kind::Request) {
            // Run each tool call on its own thread so long jobs don't block pings or other calls.
            let (server, out) = (server.clone(), out.clone());
            workers.push(std::thread::spawn(move || {
                if let Some(r) = server.handle(&msg, notify) {
                    out.send(&r);
                }
            }));
            workers.retain(|w| !w.is_finished());
        } else if let Some(r) = server.handle(&msg, notify) {
            out.send(&r);
        }
    }
    // stdin closed: let running jobs finish so their results are delivered.
    for w in workers {
        let _ = w.join();
    }
}
