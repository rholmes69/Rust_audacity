//! Chat with a local Ollama model that can use the Rust Audacity MCP tools.
//!
//! This is a small MCP *client* + Ollama bridge:
//!   you ⇄ rust_audacity_chat ⇄ Ollama (/api/chat with tools)
//!                         ⇵
//!              rust_audacity_mcp (MCP over stdio)
//!
//! Usage:
//!   rust_audacity_chat --model qwen3:8b --root C:\Users\me\Music
//!   rust_audacity_chat --model llama3.1:8b --root D:\Audio --prompt "Analyze song.mp3"
//!
//! Options:
//!   --model <name>        Ollama model with tool support (default: qwen3:8b)
//!   --ollama <url>        Ollama server (default: http://localhost:11434, or $OLLAMA_HOST)
//!   --root <dir>          folder the tools may read/write (passed to the MCP server; default: current folder)
//!   --server <path>       MCP server executable (default: rust_audacity_mcp next to this program)
//!   --server-arg <arg>    extra argument for the MCP server (repeatable), e.g. --server-arg --demucs-command --server-arg "py -m demucs"
//!   --ctx <tokens>        context window to request from Ollama (default 16384)
//!   --think               let "thinking" models reason before answering (slower)
//!   --vision              send spectrogram images to the model (use a vision model, e.g. qwen2.5vl, llama3.2-vision)
//!   --prompt <text>       run one request and exit (otherwise interactive)
//!   --max-steps <n>       maximum tool calls per request (default 12)
//!   --verbose             print full tool results
//!   --no-color            plain text output (also honours the NO_COLOR environment variable)

use std::{
    io::{BufRead, BufReader, Write},
    path::PathBuf,
    process::{Child, ChildStdin, ChildStdout, Command, Stdio},
};

use rust_audacity::ollama::{Ollama, normalize_args, to_ollama_tools, truncate};
use serde_json::{Value, json};

type R<T> = Result<T, String>;

// ---------------------------------------------------------------- output (colours can be turned off)

static NO_COLOR: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn paint(s: String) -> String {
    if !NO_COLOR.load(std::sync::atomic::Ordering::Relaxed) {
        return s;
    }
    let mut out = String::with_capacity(s.len());
    let mut it = s.chars().peekable();
    while let Some(c) = it.next() {
        if c == '\x1b' && it.peek() == Some(&'[') {
            for d in it.by_ref() {
                if d.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

macro_rules! outln { ($($t:tt)*) => { println!("{}", paint(format!($($t)*))) } }
macro_rules! out { ($($t:tt)*) => { print!("{}", paint(format!($($t)*))) } }
macro_rules! errln { ($($t:tt)*) => { eprintln!("{}", paint(format!($($t)*))) } }
macro_rules! err { ($($t:tt)*) => { eprint!("{}", paint(format!($($t)*))) } }

// ---------------------------------------------------------------- MCP client

struct McpClient {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: u64,
}

impl McpClient {
    fn start(exe: &PathBuf, args: &[String]) -> R<Self> {
        let mut child = Command::new(exe)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| format!("could not start MCP server {}: {e}", exe.display()))?;
        let stdin = child.stdin.take().unwrap();
        let stdout = BufReader::new(child.stdout.take().unwrap());
        let mut c = Self {
            child,
            stdin,
            stdout,
            next_id: 1,
        };
        let init = c.request(
            "initialize",
            json!({ "protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": { "name": "rust_audacity_chat", "version": env!("CARGO_PKG_VERSION") } }),
        )?;
        c.notify("notifications/initialized", json!({}))?;
        if init.get("serverInfo").is_none() {
            return Err("MCP server did not return serverInfo".into());
        }
        Ok(c)
    }

    fn send(&mut self, msg: &Value) -> R<()> {
        let mut line = serde_json::to_string(msg).map_err(|e| e.to_string())?;
        line.push('\n');
        self.stdin
            .write_all(line.as_bytes())
            .and_then(|_| self.stdin.flush())
            .map_err(|e| format!("MCP server closed: {e}"))
    }

    fn notify(&mut self, method: &str, params: Value) -> R<()> {
        self.send(&json!({ "jsonrpc": "2.0", "method": method, "params": params }))
    }

    /// Send a request and wait for its response, showing progress notifications.
    fn request(&mut self, method: &str, mut params: Value) -> R<Value> {
        let id = self.next_id;
        self.next_id += 1;
        if method == "tools/call" {
            params["_meta"] = json!({ "progressToken": id });
        }
        self.send(&json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }))?;
        let mut line = String::new();
        let mut showed_progress = false;
        loop {
            line.clear();
            if self
                .stdout
                .read_line(&mut line)
                .map_err(|e| e.to_string())?
                == 0
            {
                return Err("MCP server exited unexpectedly".into());
            }
            let Ok(msg) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            if msg.get("method").and_then(Value::as_str) == Some("notifications/progress") {
                let p = &msg["params"];
                if let (Some(v), Some(t)) = (p["progress"].as_f64(), p["total"].as_f64()) {
                    err!(
                        "\r   … {} {:>3.0}%   ",
                        p["message"].as_str().unwrap_or(""),
                        v / t.max(1e-9) * 100.0
                    );
                    showed_progress = true;
                }
                continue;
            }
            if msg.get("id").and_then(Value::as_u64) == Some(id) {
                if showed_progress {
                    errln!("");
                }
                if let Some(err) = msg.get("error") {
                    return Err(err["message"].as_str().unwrap_or("MCP error").to_string());
                }
                return Ok(msg["result"].clone());
            }
        }
    }
}

impl Drop for McpClient {
    fn drop(&mut self) {
        let _ = self.child.kill();
    }
}

// Ollama client and helpers are shared with the app (rust_audacity::ollama).

fn list_audio_files(root: &PathBuf) -> Vec<String> {
    let exts = [
        "wav", "mp3", "flac", "ogg", "m4a", "aac", "aif", "aiff", "opus", "webm", "mka",
    ];
    let mut out: Vec<String> = std::fs::read_dir(root)
        .map(|rd| {
            rd.filter_map(|e| e.ok().map(|e| e.path()))
                .filter(|p| {
                    p.extension().is_some_and(|e| {
                        exts.contains(&e.to_string_lossy().to_lowercase().as_str())
                    })
                })
                .filter_map(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
                .collect()
        })
        .unwrap_or_default();
    out.sort();
    out.truncate(60);
    out
}

fn system_prompt(root: &PathBuf) -> String {
    let files = list_audio_files(root);
    format!(
        "You are an audio engineer assistant with tools from Rust Audacity. You work on audio files in the folder {root}. \
Use file names relative to that folder (or absolute paths inside it).\n\
Guidelines:\n\
- Inspect before changing: use analyze_audio (it lists issues and suggested_effects) or audio_info.\n\
- Call list_effects if you need effect ids or parameter names. Effects are given as objects like {{\"effect\": \"compressor\", \"params\": {{\"ratio\": 3}}}}.\n\
- Tools always write NEW files; tell the user the output file names.\n\
- After processing, verify with analyze_audio or audio_info and report the before/after numbers briefly.\n\
- For karaoke/stems use separate_stems (engine \"demucs\" for best quality, \"builtin\" for an instant vocals/instrumental split).\n\
- Do not invent file names: only use files that exist or that a tool created.\n\
- Keep answers short and practical.\n\
Audio files currently in the folder: {}",
        if files.is_empty() {
            "(none found)".into()
        } else {
            files.join(", ")
        },
        root = root.display()
    )
}

// ---------------------------------------------------------------- main

struct Opts {
    model: String,
    ollama: String,
    root: PathBuf,
    server: PathBuf,
    server_args: Vec<String>,
    ctx: u64,
    think: bool,
    vision: bool,
    prompt: Option<String>,
    max_steps: usize,
    verbose: bool,
}

fn parse_opts() -> R<Opts> {
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(PathBuf::from))
        .unwrap_or_default();
    let mut o = Opts {
        model: "qwen3:8b".into(),
        ollama: rust_audacity::ollama::default_url(),
        root: std::env::current_dir().unwrap_or_default(),
        server: exe_dir.join(if cfg!(windows) {
            "rust_audacity_mcp.exe"
        } else {
            "rust_audacity_mcp"
        }),
        server_args: Vec::new(),
        ctx: 16384,
        think: false,
        vision: false,
        prompt: None,
        max_steps: 12,
        verbose: false,
    };
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        let mut val = |name: &str| args.next().ok_or_else(|| format!("{name} needs a value"));
        match a.as_str() {
            "--model" | "-m" => o.model = val("--model")?,
            "--ollama" => o.ollama = val("--ollama")?,
            "--root" => o.root = PathBuf::from(val("--root")?),
            "--server" => o.server = PathBuf::from(val("--server")?),
            "--server-arg" => o.server_args.push(val("--server-arg")?),
            "--ctx" => o.ctx = val("--ctx")?.parse().map_err(|_| "--ctx needs a number")?,
            "--max-steps" => {
                o.max_steps = val("--max-steps")?
                    .parse()
                    .map_err(|_| "--max-steps needs a number")?
            }
            "--prompt" | "-p" => o.prompt = Some(val("--prompt")?),
            "--think" => o.think = true,
            "--vision" => o.vision = true,
            "--verbose" | "-v" => o.verbose = true,
            "--no-color" => NO_COLOR.store(true, std::sync::atomic::Ordering::Relaxed),
            "-h" | "--help" => {
                outln!(
                    "{}",
                    include_str!("rust_audacity_chat.rs")
                        .lines()
                        .skip(2)
                        .take_while(|l| l.starts_with("//!"))
                        .map(|l| l.trim_start_matches("//!").trim_start_matches(' '))
                        .collect::<Vec<_>>()
                        .join("\n")
                );
                std::process::exit(0);
            }
            other => return Err(format!("unknown option {other} (see --help)")),
        }
    }
    o.root =
        std::fs::canonicalize(&o.root).map_err(|e| format!("--root {}: {e}", o.root.display()))?;
    Ok(o)
}

/// Run one user turn: call Ollama, execute tool calls via MCP, repeat until the model answers.
fn run_turn(
    o: &Opts,
    ollama: &Ollama,
    mcp: &mut McpClient,
    tools: &[Value],
    messages: &mut Vec<Value>,
) -> R<String> {
    for _ in 0..=o.max_steps {
        let mut body = json!({ "model": o.model, "messages": messages, "tools": tools, "stream": false, "options": { "num_ctx": o.ctx } });
        if o.think {
            body["think"] = json!(true);
        }
        err!("   … thinking\r");
        let resp = ollama.chat(&body)?;
        err!("               \r");
        let msg = resp["message"].clone();
        let calls = msg["tool_calls"].as_array().cloned().unwrap_or_default();
        let content = msg["content"].as_str().unwrap_or("").to_string();
        // Keep the assistant turn (including its tool calls) in the history.
        let mut assistant = json!({ "role": "assistant", "content": content });
        if !calls.is_empty() {
            assistant["tool_calls"] = json!(calls);
        }
        messages.push(assistant);
        if calls.is_empty() {
            return Ok(content);
        }
        if !content.trim().is_empty() {
            outln!("\x1b[2m{}\x1b[0m", content.trim());
        }
        for call in calls {
            let name = call["function"]["name"].as_str().unwrap_or("").to_string();
            let args = normalize_args(&call["function"]["arguments"]);
            outln!(
                "\x1b[36m🔧 {name}\x1b[0m {}",
                truncate(&args.to_string(), 300)
            );
            let (text, images, is_err) =
                match mcp.request("tools/call", json!({ "name": name, "arguments": args })) {
                    Ok(res) => {
                        let mut text = String::new();
                        let mut images = Vec::new();
                        for c in res["content"].as_array().cloned().unwrap_or_default() {
                            match c["type"].as_str() {
                                Some("text") => text.push_str(c["text"].as_str().unwrap_or("")),
                                Some("image") => images.push(c["data"].clone()),
                                _ => {}
                            }
                        }
                        (text, images, res["isError"].as_bool().unwrap_or(false))
                    }
                    Err(e) => (format!("Error: {e}"), Vec::new(), true),
                };
            let shown = if o.verbose {
                text.clone()
            } else {
                truncate(text.lines().next().unwrap_or(""), 200)
            };
            outln!(
                "   {}{}\x1b[0m",
                if is_err {
                    "\x1b[31m✗ "
                } else {
                    "\x1b[32m✓ "
                },
                shown
            );
            // Small models have small context windows: cap what goes back to the model.
            messages.push(
                json!({ "role": "tool", "tool_name": name, "content": truncate(&text, 6000) }),
            );
            if o.vision && !images.is_empty() {
                messages.push(json!({ "role": "user", "content": format!("(Image returned by {name}.)"), "images": images }));
            }
        }
    }
    Err(format!(
        "stopped after {} tool calls without a final answer (raise --max-steps)",
        o.max_steps
    ))
}

fn main() {
    if let Err(e) = real_main() {
        errln!("\x1b[31merror:\x1b[0m {e}");
        std::process::exit(1);
    }
}

fn real_main() -> R<()> {
    if std::env::var_os("NO_COLOR").is_some() {
        NO_COLOR.store(true, std::sync::atomic::Ordering::Relaxed);
    }
    let o = parse_opts()?;
    let ollama = Ollama::new(&o.ollama);

    // Check Ollama and the model up front for friendly errors.
    let tags = ollama.get("/api/tags")?;
    let installed: Vec<String> = tags["models"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .filter_map(|m| m["name"].as_str().map(String::from))
        .collect();
    let wanted = if o.model.contains(':') {
        o.model.clone()
    } else {
        format!("{}:latest", o.model)
    };
    if !installed.iter().any(|m| m == &wanted || m == &o.model) {
        return Err(format!(
            "model `{}` is not installed in Ollama. Run:  ollama pull {}\nInstalled: {}",
            o.model,
            o.model,
            if installed.is_empty() {
                "(none)".into()
            } else {
                installed.join(", ")
            }
        ));
    }

    let mut server_args = vec!["--root".to_string(), o.root.display().to_string()];
    server_args.extend(o.server_args.iter().cloned());
    let mut mcp = McpClient::start(&o.server, &server_args)?;
    let tools_resp = mcp.request("tools/list", json!({}))?;
    let mcp_tools = tools_resp["tools"].as_array().cloned().unwrap_or_default();
    let tools = to_ollama_tools(&mcp_tools);

    let mut messages = vec![json!({ "role": "system", "content": system_prompt(&o.root) })];

    if let Some(p) = &o.prompt {
        messages.push(json!({ "role": "user", "content": p }));
        let answer = run_turn(&o, &ollama, &mut mcp, &tools, &mut messages)?;
        outln!("{}", answer.trim());
        return Ok(());
    }

    outln!(
        "Rust Audacity × Ollama  –  model {}  –  folder {}",
        o.model,
        o.root.display()
    );
    outln!(
        "{} audio tools available. Type a request, or /tools, /files, /reset, /exit.\n",
        tools.len()
    );
    let stdin = std::io::stdin();
    loop {
        out!("\x1b[1myou>\x1b[0m ");
        let _ = std::io::stdout().flush();
        let mut line = String::new();
        if stdin.lock().read_line(&mut line).unwrap_or(0) == 0 {
            break;
        }
        let line = line.trim();
        match line {
            "" => continue,
            "/exit" | "/quit" => break,
            "/reset" => {
                messages.truncate(1);
                messages[0] = json!({ "role": "system", "content": system_prompt(&o.root) });
                outln!("(conversation cleared)");
                continue;
            }
            "/tools" => {
                for t in &mcp_tools {
                    outln!(
                        "• {} – {}",
                        t["name"].as_str().unwrap_or(""),
                        truncate(t["description"].as_str().unwrap_or(""), 110)
                    );
                }
                continue;
            }
            "/files" => {
                outln!("{}", list_audio_files(&o.root).join("\n"));
                continue;
            }
            _ => {}
        }
        messages.push(json!({ "role": "user", "content": line }));
        match run_turn(&o, &ollama, &mut mcp, &tools, &mut messages) {
            Ok(answer) => outln!("\x1b[1massistant>\x1b[0m {}\n", answer.trim()),
            Err(e) => {
                errln!("\x1b[31merror:\x1b[0m {e}\n");
                // Drop the failed user turn so the conversation stays consistent.
                while messages.len() > 1 && messages.last().is_some_and(|m| m["role"] != "user") {
                    messages.pop();
                }
                messages.pop();
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_mcp_tools() {
        let t = to_ollama_tools(&[
            json!({ "name": "audio_info", "description": "d", "inputSchema": { "type": "object", "properties": { "path": { "type": "string" } } } }),
        ]);
        assert_eq!(t[0]["type"], "function");
        assert_eq!(t[0]["function"]["name"], "audio_info");
        assert_eq!(
            t[0]["function"]["parameters"]["properties"]["path"]["type"],
            "string"
        );
    }

    #[test]
    fn string_arguments_are_parsed() {
        assert_eq!(
            normalize_args(&json!("{\"path\":\"a.wav\"}")),
            json!({ "path": "a.wav" })
        );
        assert_eq!(
            normalize_args(&json!({ "path": "a.wav" })),
            json!({ "path": "a.wav" })
        );
        assert_eq!(normalize_args(&Value::Null), json!({}));
    }
}
