//! Model Context Protocol server for Rust Audacity.
//!
//! The protocol logic and all tools live here so they can be served two ways:
//! * over **stdio** by the `rust_audacity_mcp` program (for Claude Desktop and others), and
//! * over **Streamable HTTP** by the desktop app itself ([`http`]), so agents can connect by URL.

pub mod app_tools;
pub mod http;
pub mod tools;

use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

use serde_json::{Value, json};

use crate::{bridge::BridgeInfo, dsp::separation::CancelHandle};

pub use tools::Output;

pub const SERVER_NAME: &str = "rust-audacity";
pub const SUPPORTED_VERSIONS: [&str; 4] = ["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];
pub const DEFAULT_VERSION: &str = "2025-06-18";

pub const INSTRUCTIONS: &str = "Rust Audacity audio tools. Work on audio files by path (absolute paths are safest). \
Typical workflow: (1) audio_info / analyze_audio to inspect a file – analyze_audio also reports problems \
(clipping, hum, noise, loudness) with suggested effects; (2) list_effects to see every effect and its \
parameters; (3) apply_effects / edit_audio / convert_audio / mix_audio to write a NEW output file \
(inputs are never modified unless output equals input and overwrite=true); (4) analyze the output to verify. \
separate_stems splits music into vocals/drums/bass/other (AI via Demucs, or the instant 'builtin' engine for \
vocals/instrumental). render_spectrogram returns a PNG image you can look at. transcribe_audio turns speech into \
text with word timings (Whisper). Times are in seconds. \
The app_* tools instead drive the Rust Audacity window the user has open (start with app_status): open files, \
select, edit, apply effects, duck music under speech, set volume envelopes, play, export, save, and \
transcribe/edit speech by deleting words (app_transcript). Changes there are visible to the user and can be \
undone (app_edit op=undo).";

/// Receives one log line per finished tool call.
pub type LogFn = Box<dyn Fn(&str) + Send + Sync>;

/// Where notifications (progress) for one request go.
pub type Notify = Arc<dyn Fn(Value) + Send + Sync>;

/// Sends `notifications/progress` when the client supplied a progress token.
#[derive(Clone)]
pub struct Progress {
    notify: Notify,
    token: Option<Value>,
}

impl Progress {
    pub fn new(notify: Notify, token: Option<Value>) -> Self {
        Self { notify, token }
    }

    /// A progress reporter that discards everything.
    pub fn none() -> Self {
        Self {
            notify: Arc::new(|_| {}),
            token: None,
        }
    }

    pub fn report(&self, progress: f64, total: Option<f64>, message: &str) {
        if let Some(t) = &self.token {
            let mut p = json!({ "progressToken": t, "progress": progress, "message": message });
            if let Some(total) = total {
                p["total"] = json!(total);
            }
            (self.notify)(
                json!({ "jsonrpc": "2.0", "method": "notifications/progress", "params": p }),
            );
        }
    }
}

#[derive(Clone, Debug)]
pub struct Config {
    /// Only allow file access inside this folder.
    pub root: Option<PathBuf>,
    pub demucs_command: String,
    /// Python used for Whisper transcription.
    pub python: String,
    /// Bridge of the app to drive with the app_* tools (None = find it via the discovery file).
    pub bridge: Option<BridgeInfo>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            root: None,
            demucs_command: if cfg!(windows) {
                "python -m demucs".into()
            } else {
                "demucs".into()
            },
            python: crate::whisper::default_python().into(),
            bridge: None,
        }
    }
}

struct InFlight {
    cancel: CancelHandle,
    cancelled: Arc<AtomicBool>,
}

pub fn response(id: &Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

pub fn error(id: &Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

/// What a message turned out to be, before it is handled.
pub enum Kind {
    /// A request that needs a response (`id` present).
    Request,
    /// A notification or a response from the client: no reply.
    Notification,
    /// Not JSON-RPC at all.
    Invalid,
}

pub fn kind(msg: &Value) -> Kind {
    match (msg.get("method").and_then(Value::as_str), msg.get("id")) {
        (Some(_), Some(id)) if !id.is_null() => Kind::Request,
        (Some(_), _) => Kind::Notification,
        (None, Some(_)) => Kind::Notification, // a response to a server request
        _ => Kind::Invalid,
    }
}

/// The MCP server: protocol handling shared by every transport.
pub struct Server {
    pub cfg: Mutex<Config>,
    inflight: Mutex<HashMap<String, InFlight>>,
    /// Log line for each finished tool call (stdio prints these to stderr).
    pub log: Option<LogFn>,
}

impl Server {
    pub fn new(cfg: Config) -> Arc<Self> {
        Arc::new(Self {
            cfg: Mutex::new(cfg),
            inflight: Mutex::new(HashMap::new()),
            log: None,
        })
    }

    pub fn with_log(cfg: Config, log: impl Fn(&str) + Send + Sync + 'static) -> Arc<Self> {
        Arc::new(Self {
            cfg: Mutex::new(cfg),
            inflight: Mutex::new(HashMap::new()),
            log: Some(Box::new(log)),
        })
    }

    fn config(&self) -> Config {
        self.cfg.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Handle one JSON-RPC message (blocking: tool calls run to completion). Returns the
    /// response, or `None` for notifications and for requests the client cancelled.
    pub fn handle(&self, msg: &Value, notify: Notify) -> Option<Value> {
        let method = msg.get("method").and_then(Value::as_str);
        let params = msg.get("params").cloned().unwrap_or(json!({}));
        let id = msg.get("id").cloned().filter(|v| !v.is_null());
        let (Some(method), Some(id)) = (method, id) else {
            if method == Some("notifications/cancelled")
                && let Some(rid) = params.get("requestId")
                && let Some(f) = self.inflight.lock().unwrap().get(&rid.to_string())
            {
                f.cancelled.store(true, Ordering::SeqCst);
                f.cancel.cancel();
            }
            return if method.is_none() && msg.get("id").is_none() {
                Some(error(&Value::Null, -32600, "invalid request"))
            } else {
                None
            };
        };
        Some(match method {
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
                response(
                    &id,
                    json!({
                        "protocolVersion": version,
                        "capabilities": { "tools": { "listChanged": false } },
                        "serverInfo": { "name": SERVER_NAME, "title": "Rust Audacity", "version": env!("CARGO_PKG_VERSION") },
                        "instructions": INSTRUCTIONS,
                    }),
                )
            }
            "ping" => response(&id, json!({})),
            "tools/list" => response(&id, json!({ "tools": tools::definitions() })),
            "tools/call" => {
                let name = params
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                if !tools::exists(&name) {
                    return Some(error(&id, -32602, &format!("unknown tool: {name}")));
                }
                let args = params.get("arguments").cloned().unwrap_or(json!({}));
                let progress = Progress::new(
                    notify,
                    params
                        .get("_meta")
                        .and_then(|m| m.get("progressToken"))
                        .cloned(),
                );
                let key = id.to_string();
                let flight = InFlight {
                    cancel: CancelHandle::default(),
                    cancelled: Arc::default(),
                };
                let (cancel, cancelled) = (flight.cancel.clone(), flight.cancelled.clone());
                self.inflight.lock().unwrap().insert(key.clone(), flight);
                let cfg = self.config();
                let started = std::time::Instant::now();
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    tools::call(&name, &args, &cfg, &progress, &cancel)
                }))
                .unwrap_or_else(|_| Err("internal error (panic) while running the tool".into()));
                self.inflight.lock().unwrap().remove(&key);
                if let Some(log) = &self.log {
                    log(&format!(
                        "{name} finished in {:.2}s ({})",
                        started.elapsed().as_secs_f32(),
                        if result.is_ok() { "ok" } else { "error" }
                    ));
                }
                if cancelled.load(Ordering::SeqCst) {
                    return None; // client cancelled: no response
                }
                response(&id, tools::to_result(result))
            }
            "resources/list" => response(&id, json!({ "resources": [] })),
            "resources/templates/list" => response(&id, json!({ "resourceTemplates": [] })),
            "prompts/list" => response(&id, json!({ "prompts": [] })),
            _ => error(&id, -32601, &format!("method not found: {method}")),
        })
    }
}
