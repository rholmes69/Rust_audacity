//! A small Ollama client (`/api/tags`, `/api/chat` with tools) and the tool-calling loop,
//! shared by the in-app assistant and the `rust_audacity_chat` terminal program.

use std::{
    sync::atomic::{AtomicBool, Ordering},
    time::Duration,
};

use serde_json::{Value, json};

pub type R<T> = Result<T, String>;

pub const DEFAULT_URL: &str = "http://localhost:11434";

/// `$OLLAMA_HOST` (with `http://` added if missing) or the default.
pub fn default_url() -> String {
    std::env::var("OLLAMA_HOST")
        .map(|h| {
            if h.starts_with("http") {
                h
            } else {
                format!("http://{h}")
            }
        })
        .unwrap_or_else(|_| DEFAULT_URL.into())
}

pub struct Ollama {
    agent: ureq::Agent,
    base: String,
}

impl Ollama {
    pub fn new(base: &str) -> Self {
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(1800)))
            .http_status_as_error(false)
            .proxy(None) // Ollama is local; never route through a system proxy
            .build()
            .into();
        Self {
            agent,
            base: base.trim_end_matches('/').to_string(),
        }
    }

    pub fn get(&self, path: &str) -> R<Value> {
        let mut resp = self
            .agent
            .get(&format!("{}{path}", self.base))
            .call()
            .map_err(|e| self.conn_err(e))?;
        resp.body_mut()
            .read_json::<Value>()
            .map_err(|e| e.to_string())
    }

    /// Installed model names.
    pub fn models(&self) -> R<Vec<String>> {
        let tags = self.get("/api/tags")?;
        Ok(tags["models"]
            .as_array()
            .cloned()
            .unwrap_or_default()
            .iter()
            .filter_map(|m| m["name"].as_str().map(String::from))
            .collect())
    }

    pub fn chat(&self, body: &Value) -> R<Value> {
        let mut resp = self
            .agent
            .post(&format!("{}/api/chat", self.base))
            .send_json(body)
            .map_err(|e| self.conn_err(e))?;
        let status = resp.status();
        let text = resp
            .body_mut()
            .read_to_string()
            .map_err(|e| e.to_string())?;
        let v: Value = serde_json::from_str(&text).unwrap_or(json!({ "error": text }));
        if !status.is_success() || v.get("error").is_some() {
            return Err(format!(
                "Ollama error ({status}): {}",
                v["error"].as_str().unwrap_or(&text)
            ));
        }
        Ok(v)
    }

    fn conn_err(&self, e: ureq::Error) -> String {
        format!(
            "cannot reach Ollama at {} ({e}). Is Ollama running? Start it with `ollama serve` or the Ollama app.",
            self.base
        )
    }
}

/// MCP tool definitions → Ollama/OpenAI-style function tools.
pub fn to_ollama_tools(mcp_tools: &[Value]) -> Vec<Value> {
    mcp_tools
        .iter()
        .map(|t| {
            json!({
                "type": "function",
                "function": {
                    "name": t["name"],
                    "description": t["description"],
                    "parameters": t.get("inputSchema").cloned().unwrap_or(json!({ "type": "object", "properties": {} })),
                }
            })
        })
        .collect()
}

/// Some models return arguments as a JSON string instead of an object.
pub fn normalize_args(v: &Value) -> Value {
    match v {
        Value::String(s) => serde_json::from_str(s).unwrap_or(json!({})),
        Value::Null => json!({}),
        other => other.clone(),
    }
}

pub fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        format!("{}… [truncated]", s.chars().take(max).collect::<String>())
    }
}

/// What happens during a turn (for showing progress).
#[derive(Debug, Clone)]
pub enum TurnEvent {
    /// Text the model wrote alongside tool calls.
    Note(String),
    ToolStart {
        name: String,
        args: Value,
    },
    ToolDone {
        name: String,
        text: String,
        is_error: bool,
    },
}

pub struct TurnOptions {
    pub model: String,
    pub ctx: u64,
    pub think: bool,
    pub max_steps: usize,
}

/// Run one user turn: ask the model, run the tools it calls (via `call_tool`, which returns
/// (text, is_error)), repeat until it answers. `messages` gets the whole exchange.
pub fn run_turn(
    ollama: &Ollama,
    o: &TurnOptions,
    tools: &[Value],
    messages: &mut Vec<Value>,
    call_tool: &mut dyn FnMut(&str, &Value) -> (String, bool),
    on_event: &mut dyn FnMut(TurnEvent),
    cancel: &AtomicBool,
) -> R<String> {
    for _ in 0..=o.max_steps {
        if cancel.load(Ordering::Relaxed) {
            return Err("stopped".into());
        }
        let mut body = json!({ "model": o.model, "messages": messages, "tools": tools, "stream": false, "options": { "num_ctx": o.ctx } });
        if o.think {
            body["think"] = json!(true);
        }
        let resp = ollama.chat(&body)?;
        let msg = resp["message"].clone();
        let calls = msg["tool_calls"].as_array().cloned().unwrap_or_default();
        let content = msg["content"].as_str().unwrap_or("").to_string();
        let mut assistant = json!({ "role": "assistant", "content": content });
        if !calls.is_empty() {
            assistant["tool_calls"] = json!(calls);
        }
        messages.push(assistant);
        if calls.is_empty() {
            return Ok(content);
        }
        if !content.trim().is_empty() {
            on_event(TurnEvent::Note(content.trim().to_string()));
        }
        for call in calls {
            if cancel.load(Ordering::Relaxed) {
                return Err("stopped".into());
            }
            let name = call["function"]["name"].as_str().unwrap_or("").to_string();
            let args = normalize_args(&call["function"]["arguments"]);
            on_event(TurnEvent::ToolStart {
                name: name.clone(),
                args: args.clone(),
            });
            let (text, is_error) = call_tool(&name, &args);
            on_event(TurnEvent::ToolDone {
                name: name.clone(),
                text: text.clone(),
                is_error,
            });
            // Small models have small context windows: cap what goes back to the model.
            messages.push(
                json!({ "role": "tool", "tool_name": name, "content": truncate(&text, 6000) }),
            );
        }
    }
    Err(format!(
        "stopped after {} tool calls without a final answer",
        o.max_steps
    ))
}
