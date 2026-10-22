//! Agent bridge: lets the MCP server drive a running Rust Audacity window.
//!
//! The app listens on `127.0.0.1` (a random free port) and writes the port plus a random
//! secret token to a small JSON file in the user's temp folder. The MCP server reads that
//! file and sends one JSON request per line:
//!
//! ```text
//! → {"token":"…","cmd":"status","args":{}}
//! ← {"ok":true,"result":{…}}      or      {"ok":false,"error":"…"}
//! ```
//!
//! Only processes running as the same user can read the token, and the socket is bound to
//! the loopback interface only.

use std::{
    io::{BufRead, BufReader, Write},
    net::{TcpListener, TcpStream},
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{Receiver, Sender, channel},
    },
    time::Duration,
};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// Contents of the discovery file.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct BridgeInfo {
    pub port: u16,
    pub token: String,
    pub pid: u32,
    pub version: String,
}

/// Where the running app advertises itself. `RUST_AUDACITY_BRIDGE_FILE` overrides it.
pub fn info_path() -> PathBuf {
    std::env::var_os("RUST_AUDACITY_BRIDGE_FILE")
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::temp_dir().join("rust_audacity_bridge.json"))
}

/// A request from an agent, waiting for the UI thread to answer.
pub struct Request {
    pub cmd: String,
    pub args: Value,
    reply: Sender<Result<Value, String>>,
}

impl Request {
    pub fn respond(self, result: Result<Value, String>) {
        let _ = self.reply.send(result);
    }
}

/// The listening side, owned by the app.
pub struct Server {
    pub info: BridgeInfo,
    rx: Receiver<Request>,
    stop: Arc<AtomicBool>,
}

fn random_token() -> String {
    use std::hash::{BuildHasher, Hasher};
    let mut out = String::new();
    for i in 0..2u64 {
        // RandomState is seeded from the OS's random source.
        let mut h = std::collections::hash_map::RandomState::new().build_hasher();
        h.write_u64(i);
        h.write_u128(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0),
        );
        h.write_u32(std::process::id());
        out.push_str(&format!("{:016x}", h.finish()));
    }
    out
}

impl Server {
    /// Start listening and write the discovery file. `notify` is called whenever a request
    /// arrives (the app uses it to wake its UI thread).
    pub fn start(notify: impl Fn() + Send + Sync + 'static) -> std::io::Result<Server> {
        let listener = TcpListener::bind(("127.0.0.1", 0))?;
        let info = BridgeInfo {
            port: listener.local_addr()?.port(),
            token: random_token(),
            pid: std::process::id(),
            version: env!("CARGO_PKG_VERSION").into(),
        };
        let (tx, rx) = channel();
        let stop = Arc::new(AtomicBool::new(false));
        let notify: Arc<dyn Fn() + Send + Sync> = Arc::new(notify);
        let token = info.token.clone();
        let stop2 = stop.clone();
        std::thread::Builder::new()
            .name("agent-bridge".into())
            .spawn(move || {
                for conn in listener.incoming() {
                    if stop2.load(Ordering::Relaxed) {
                        break;
                    }
                    let Ok(conn) = conn else { continue };
                    let (tx, token, notify) = (tx.clone(), token.clone(), notify.clone());
                    std::thread::spawn(move || serve_connection(conn, &tx, &token, &*notify));
                }
            })?;
        let json = serde_json::to_string_pretty(&info).unwrap_or_default();
        std::fs::write(info_path(), json)?;
        Ok(Server { info, rx, stop })
    }

    /// Requests that have arrived since the last call.
    pub fn pending(&self) -> Vec<Request> {
        self.rx.try_iter().collect()
    }

    /// Stop accepting connections and remove the discovery file (if it is still ours).
    pub fn shutdown(&self) {
        self.stop.store(true, Ordering::Relaxed);
        let path = info_path();
        if let Ok(s) = std::fs::read_to_string(&path)
            && let Ok(info) = serde_json::from_str::<BridgeInfo>(&s)
            && info.pid == self.info.pid
            && info.token == self.info.token
        {
            let _ = std::fs::remove_file(path);
        }
        // Wake the accept loop so it notices `stop`.
        let _ = TcpStream::connect(("127.0.0.1", self.info.port));
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn serve_connection(conn: TcpStream, tx: &Sender<Request>, token: &str, notify: &dyn Fn()) {
    let Ok(mut writer) = conn.try_clone() else {
        return;
    };
    let reader = BufReader::new(conn);
    for line in reader.lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        let reply = match serde_json::from_str::<Value>(&line) {
            Err(e) => json!({ "ok": false, "error": format!("bad JSON: {e}") }),
            Ok(msg) if msg.get("token").and_then(Value::as_str) != Some(token) => {
                json!({ "ok": false, "error": "invalid token" })
            }
            Ok(msg) => {
                let (rtx, rrx) = channel();
                let req = Request {
                    cmd: msg["cmd"].as_str().unwrap_or("").to_string(),
                    args: msg.get("args").cloned().unwrap_or(json!({})),
                    reply: rtx,
                };
                if tx.send(req).is_err() {
                    break;
                }
                notify();
                match rrx.recv_timeout(Duration::from_secs(60)) {
                    Ok(Ok(v)) => json!({ "ok": true, "result": v }),
                    Ok(Err(e)) => json!({ "ok": false, "error": e }),
                    Err(_) => {
                        json!({ "ok": false, "error": "Rust Audacity did not answer in time (is a dialog open?)" })
                    }
                }
            }
        };
        let mut s = reply.to_string();
        s.push('\n');
        if writer.write_all(s.as_bytes()).is_err() {
            break;
        }
    }
}

/// The calling side, used by the MCP server.
pub struct Client {
    stream: BufReader<TcpStream>,
    token: String,
}

impl Client {
    /// Connect to the running app using the discovery file.
    pub fn connect() -> Result<Client, String> {
        const NOT_RUNNING: &str = "Rust Audacity isn't open. Start the Rust Audacity app first \
             (and keep Agents ▸ Allow AI agents to control this window turned on).";
        let text = std::fs::read_to_string(info_path()).map_err(|_| NOT_RUNNING.to_string())?;
        let info: BridgeInfo = serde_json::from_str(&text).map_err(|_| NOT_RUNNING.to_string())?;
        let stream =
            TcpStream::connect_timeout(&([127, 0, 0, 1], info.port).into(), Duration::from_secs(2))
                .map_err(|_| NOT_RUNNING.to_string())?;
        stream
            .set_read_timeout(Some(Duration::from_secs(75)))
            .map_err(|e| e.to_string())?;
        Ok(Client {
            stream: BufReader::new(stream),
            token: info.token,
        })
    }

    /// Send one command and wait for the answer.
    pub fn call(&mut self, cmd: &str, args: Value) -> Result<Value, String> {
        let mut msg = json!({ "token": self.token, "cmd": cmd, "args": args }).to_string();
        msg.push('\n');
        self.stream
            .get_mut()
            .write_all(msg.as_bytes())
            .map_err(|e| format!("lost connection to Rust Audacity: {e}"))?;
        let mut line = String::new();
        self.stream
            .read_line(&mut line)
            .map_err(|e| format!("lost connection to Rust Audacity: {e}"))?;
        if line.is_empty() {
            return Err("Rust Audacity closed the connection".into());
        }
        let v: Value = serde_json::from_str(&line).map_err(|e| e.to_string())?;
        if v["ok"].as_bool() == Some(true) {
            Ok(v["result"].clone())
        } else {
            Err(v["error"].as_str().unwrap_or("unknown error").to_string())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_and_token_check() {
        let file = std::env::temp_dir().join(format!("ra_bridge_test_{}.json", std::process::id()));
        // SAFETY: tests in this crate don't read this variable concurrently.
        unsafe { std::env::set_var("RUST_AUDACITY_BRIDGE_FILE", &file) };
        let server = Server::start(|| {}).unwrap();
        let info: BridgeInfo =
            serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
        assert_eq!(info, server.info);

        // Answer requests from a "UI thread".
        let handle = std::thread::spawn(move || {
            let mut handled = 0;
            while handled < 2 {
                for r in server.pending() {
                    let res = match r.cmd.as_str() {
                        "echo" => Ok(r.args.clone()),
                        _ => Err(format!("unknown command {}", r.cmd)),
                    };
                    r.respond(res);
                    handled += 1;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            server
        });
        let mut c = Client::connect().unwrap();
        assert_eq!(c.call("echo", json!({"x": 1})).unwrap(), json!({"x": 1}));
        assert_eq!(
            c.call("nope", json!({})).unwrap_err(),
            "unknown command nope"
        );
        let server = handle.join().unwrap();

        // A wrong token is refused without reaching the app.
        let mut raw = TcpStream::connect(("127.0.0.1", info.port)).unwrap();
        raw.write_all(b"{\"token\":\"wrong\",\"cmd\":\"echo\"}\n")
            .unwrap();
        let mut line = String::new();
        BufReader::new(raw).read_line(&mut line).unwrap();
        assert!(line.contains("invalid token"));
        assert!(server.pending().is_empty());

        drop(server);
        assert!(!file.exists(), "discovery file removed on shutdown");
        assert!(Client::connect().err().unwrap().contains("isn't open"));
    }
}
