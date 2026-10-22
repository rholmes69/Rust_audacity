//! Rust Audacity MCP server.
//!
//! Exposes the Rust Audacity audio engine to AI agents (Claude Desktop, Claude Code,
//! Cursor, …) over the Model Context Protocol using the stdio transport:
//! newline-delimited JSON-RPC 2.0 on stdin/stdout. Logs go to stderr.
//!
//! Usage:  rust_audacity_mcp [--root <dir>] [--demucs-command "<cmd>"] [--python <python>]
//!   --root            only allow reading/writing files inside this folder (recommended)
//!   --python          Python with faster-whisper installed, for transcribe_audio (default python/python3)
//!   --demucs-command  command used for AI stem separation (default: "python -m demucs" on
//!                     Windows, "demucs" elsewhere)

mod app_tools;
mod tools;

use std::{
    collections::HashMap,
    io::{BufRead, Write},
    path::PathBuf,
    sync::{Arc, Mutex},
};

use rust_audacity::dsp::separation::CancelHandle;
use serde_json::{Value, json};

const SERVER_NAME: &str = "rust-audacity";
const SUPPORTED_VERSIONS: [&str; 4] = ["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];
const DEFAULT_VERSION: &str = "2025-06-18";

const INSTRUCTIONS: &str = "Rust Audacity audio tools. Work on audio files by path (absolute paths are safest). \
Typical workflow: (1) audio_info / analyze_audio to inspect a file – analyze_audio also reports problems \
(clipping, hum, noise, loudness) with suggested effects; (2) list_effects to see every effect and its \
parameters; (3) apply_effects / edit_audio / convert_audio / mix_audio to write a NEW output WAV file \
(inputs are never modified unless output equals input and overwrite=true); (4) analyze the output to verify. \
separate_stems splits music into vocals/drums/bass/other (AI via Demucs, or the instant 'builtin' engine for \
vocals/instrumental). render_spectrogram returns a PNG image you can look at. transcribe_audio turns speech into \
text with word timings (Whisper). Times are in seconds. \
The app_* tools instead drive the Rust Audacity window the user has open (start with app_status): open files, \
select, edit, apply effects, play, export, save, and transcribe/edit speech by deleting words (app_transcript). \
Changes there are visible to the user and can be undone (app_edit op=undo).";

/// Shared, line-oriented stdout writer (one JSON message per line).
#[derive(Clone)]
pub struct Out(Arc<Mutex<std::io::Stdout>>);

impl Out {
    pub fn send(&self, msg: &Value) {
        let mut s = serde_json::to_string(msg).unwrap_or_default();
        s.push('\n');
        let mut o = self.0.lock().unwrap();
        let _ = o.write_all(s.as_bytes());
        let _ = o.flush();
    }
}

/// Sends `notifications/progress` when the client supplied a progress token.
#[derive(Clone)]
pub struct Progress {
    out: Out,
    token: Option<Value>,
}

impl Progress {
    pub fn report(&self, progress: f64, total: Option<f64>, message: &str) {
        if let Some(t) = &self.token {
            let mut p = json!({ "progressToken": t, "progress": progress, "message": message });
            if let Some(total) = total {
                p["total"] = json!(total);
            }
            self.out.send(
                &json!({ "jsonrpc": "2.0", "method": "notifications/progress", "params": p }),
            );
        }
    }
}

#[derive(Clone)]
pub struct Config {
    pub root: Option<PathBuf>,
    pub demucs_command: String,
    /// Python used for Whisper transcription.
    pub python: String,
}

struct InFlight {
    cancel: CancelHandle,
    cancelled: Arc<std::sync::atomic::AtomicBool>,
}

fn response(id: &Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

fn error(id: &Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

fn parse_args() -> Result<Config, String> {
    let mut cfg = Config {
        root: None,
        demucs_command: if cfg!(windows) {
            "python -m demucs".into()
        } else {
            "demucs".into()
        },
        python: rust_audacity::whisper::default_python().into(),
    };
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
    let out = Out(Arc::new(Mutex::new(std::io::stdout())));
    let inflight: Arc<Mutex<HashMap<String, InFlight>>> = Arc::default();
    let mut workers = Vec::new();

    let stdin = std::io::stdin();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        let msg: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(e) => {
                out.send(&error(&Value::Null, -32700, &format!("parse error: {e}")));
                continue;
            }
        };
        let method = msg
            .get("method")
            .and_then(Value::as_str)
            .map(str::to_string);
        let id = msg.get("id").cloned();
        let params = msg.get("params").cloned().unwrap_or(json!({}));
        let (Some(method), id) = (method, id) else {
            continue; // a response to something we never send – ignore
        };

        // Notifications (no id).
        let Some(id) = id else {
            if method == "notifications/cancelled"
                && let Some(rid) = params.get("requestId")
                && let Some(f) = inflight.lock().unwrap().get(&rid.to_string())
            {
                f.cancelled.store(true, std::sync::atomic::Ordering::SeqCst);
                f.cancel.cancel();
                eprintln!("cancel requested for {rid}");
            }
            continue;
        };

        match method.as_str() {
            "initialize" => {
                let requested = params
                    .get("protocolVersion")
                    .and_then(Value::as_str)
                    .unwrap_or(DEFAULT_VERSION);
                let version = if SUPPORTED_VERSIONS.contains(&requested) {
                    requested
                } else {
                    DEFAULT_VERSION
                };
                out.send(&response(
                    &id,
                    json!({
                        "protocolVersion": version,
                        "capabilities": { "tools": { "listChanged": false } },
                        "serverInfo": { "name": SERVER_NAME, "title": "Rust Audacity", "version": env!("CARGO_PKG_VERSION") },
                        "instructions": INSTRUCTIONS,
                    }),
                ));
            }
            "ping" => out.send(&response(&id, json!({}))),
            "tools/list" => out.send(&response(&id, json!({ "tools": tools::definitions() }))),
            "tools/call" => {
                let name = params
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                if !tools::exists(&name) {
                    out.send(&error(&id, -32602, &format!("unknown tool: {name}")));
                    continue;
                }
                let args = params.get("arguments").cloned().unwrap_or(json!({}));
                let progress = Progress {
                    out: out.clone(),
                    token: params
                        .get("_meta")
                        .and_then(|m| m.get("progressToken"))
                        .cloned(),
                };
                let key = id.to_string();
                let flight = InFlight {
                    cancel: CancelHandle::default(),
                    cancelled: Arc::default(),
                };
                let (cancel, cancelled) = (flight.cancel.clone(), flight.cancelled.clone());
                inflight.lock().unwrap().insert(key.clone(), flight);
                let (out2, cfg2, inflight2) = (out.clone(), cfg.clone(), inflight.clone());
                // Run each tool call on its own thread so long jobs don't block pings or other calls.
                workers.push(std::thread::spawn(move || {
                    let started = std::time::Instant::now();
                    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        tools::call(&name, &args, &cfg2, &progress, &cancel)
                    }))
                    .unwrap_or_else(|_| {
                        Err("internal error (panic) while running the tool".into())
                    });
                    inflight2.lock().unwrap().remove(&key);
                    eprintln!(
                        "{name} finished in {:.2}s ({})",
                        started.elapsed().as_secs_f32(),
                        if result.is_ok() { "ok" } else { "error" }
                    );
                    if cancelled.load(std::sync::atomic::Ordering::SeqCst) {
                        return; // client cancelled: no response
                    }
                    out2.send(&response(&id, tools::to_result(result)));
                }));
                workers.retain(|w| !w.is_finished());
            }
            "resources/list" => out.send(&response(&id, json!({ "resources": [] }))),
            "prompts/list" => out.send(&response(&id, json!({ "prompts": [] }))),
            _ => out.send(&error(&id, -32601, &format!("method not found: {method}"))),
        }
    }
    // stdin closed: let running jobs finish so their results are delivered.
    for w in workers {
        let _ = w.join();
    }
}
