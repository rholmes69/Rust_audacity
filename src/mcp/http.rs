//! MCP over Streamable HTTP, served by the desktop app on `http://127.0.0.1:<port>/mcp`.
//!
//! * `POST /mcp` takes a JSON-RPC message (or a batch). Requests are answered with
//!   `application/json`, or — when the client accepts `text/event-stream` and calls a tool —
//!   with a short SSE stream that carries progress notifications and then the result.
//!   Notifications get `202 Accepted`.
//! * `GET /mcp` returns 405 (this server doesn't push messages on its own) and
//!   `DELETE /mcp` ends a session.
//!
//! Security: the socket is bound to 127.0.0.1 only; the `Host` and `Origin` headers must
//! name this computer (blocks DNS-rebinding attacks from web pages); and when a token is
//! set every request needs `Authorization: Bearer <token>`.

use std::{
    collections::HashMap,
    io::{BufRead, BufReader, Read, Write},
    net::{TcpListener, TcpStream},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use serde_json::{Value, json};

use super::{Kind, Notify, Server, kind};

const MAX_HEADER: usize = 64 * 1024;
const MAX_BODY: usize = 16 * 1024 * 1024;

#[derive(Clone, Debug)]
pub struct HttpOptions {
    /// 0 picks a free port.
    pub port: u16,
    /// Required bearer token (None = no authentication; still localhost-only).
    pub token: Option<String>,
}

/// Who has been talking to the server (for the app's status display).
#[derive(Clone, Debug, Default)]
pub struct Activity {
    pub requests: u64,
    pub last_client: Option<String>,
    pub last_tool: Option<String>,
    pub last_seen: Option<Instant>,
}

pub struct HttpServer {
    pub port: u16,
    stop: Arc<AtomicBool>,
    activity: Arc<Mutex<Activity>>,
}

/// A random hex token (128 bits from the OS-seeded hasher).
pub fn random_token() -> String {
    use std::hash::{BuildHasher, Hasher};
    let mut out = String::new();
    for i in 0..2u64 {
        let mut h = std::collections::hash_map::RandomState::new().build_hasher();
        h.write_u64(i);
        h.write_u128(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0),
        );
        out.push_str(&format!("{:016x}", h.finish()));
    }
    out
}

impl HttpServer {
    pub fn start(server: Arc<Server>, opts: HttpOptions) -> std::io::Result<HttpServer> {
        let listener = TcpListener::bind(("127.0.0.1", opts.port))?;
        let port = listener.local_addr()?.port();
        let stop = Arc::new(AtomicBool::new(false));
        let activity = Arc::new(Mutex::new(Activity::default()));
        let (stop2, act2) = (stop.clone(), activity.clone());
        let token = opts.token.clone();
        std::thread::Builder::new()
            .name("mcp-http".into())
            .spawn(move || {
                for conn in listener.incoming() {
                    if stop2.load(Ordering::Relaxed) {
                        break;
                    }
                    let Ok(conn) = conn else { continue };
                    let (server, token, act) = (server.clone(), token.clone(), act2.clone());
                    std::thread::spawn(move || {
                        let _ = serve(conn, &server, token.as_deref(), &act);
                    });
                }
            })?;
        Ok(HttpServer {
            port,
            stop,
            activity,
        })
    }

    pub fn url(&self) -> String {
        format!("http://127.0.0.1:{}/mcp", self.port)
    }

    pub fn activity(&self) -> Activity {
        self.activity
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    pub fn shutdown(&self) {
        if !self.stop.swap(true, Ordering::Relaxed) {
            // Wake the accept loop so it notices `stop`.
            let _ = TcpStream::connect(("127.0.0.1", self.port));
        }
    }
}

impl Drop for HttpServer {
    fn drop(&mut self) {
        self.shutdown();
    }
}

struct Request {
    method: String,
    path: String,
    headers: HashMap<String, String>,
    body: Vec<u8>,
}

fn read_request(stream: &mut BufReader<TcpStream>) -> Result<Option<Request>, String> {
    let mut line = String::new();
    if stream.read_line(&mut line).map_err(|e| e.to_string())? == 0 {
        return Ok(None);
    }
    let mut parts = line.split_whitespace();
    let method = parts.next().unwrap_or("").to_ascii_uppercase();
    let target = parts.next().unwrap_or("/").to_string();
    let path = target.split('?').next().unwrap_or("/").to_string();
    let mut headers = HashMap::new();
    let mut total = line.len();
    loop {
        let mut h = String::new();
        let n = stream.read_line(&mut h).map_err(|e| e.to_string())?;
        total += n;
        if n == 0 || total > MAX_HEADER {
            return Err("headers too large or connection closed".into());
        }
        let h = h.trim_end();
        if h.is_empty() {
            break;
        }
        if let Some((k, v)) = h.split_once(':') {
            headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
        }
    }
    let len: usize = headers
        .get("content-length")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    if len > MAX_BODY {
        return Err("body too large".into());
    }
    let mut body = vec![0; len];
    stream.read_exact(&mut body).map_err(|e| e.to_string())?;
    Ok(Some(Request {
        method,
        path,
        headers,
        body,
    }))
}

fn write_response(
    w: &mut TcpStream,
    status: u16,
    reason: &str,
    headers: &[(&str, String)],
    body: &[u8],
) -> std::io::Result<()> {
    let mut head = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Length: {}\r\nConnection: close\r\n",
        body.len()
    );
    for (k, v) in headers {
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    head.push_str("\r\n");
    w.write_all(head.as_bytes())?;
    w.write_all(body)?;
    w.flush()
}

fn json_error(
    w: &mut TcpStream,
    status: u16,
    reason: &str,
    msg: &str,
    extra: &[(&str, String)],
) -> std::io::Result<()> {
    let body = json!({ "jsonrpc": "2.0", "id": null, "error": { "code": -32000, "message": msg } })
        .to_string();
    let mut h = vec![("Content-Type", "application/json".to_string())];
    h.extend(extra.iter().cloned());
    write_response(w, status, reason, &h, body.as_bytes())
}

/// True for `127.0.0.1[:port]`, `localhost[:port]` and `[::1][:port]`.
fn is_local_host(hostport: &str) -> bool {
    let h = hostport.trim().to_ascii_lowercase();
    let host = if let Some(rest) = h.strip_prefix('[') {
        rest.split(']').next().unwrap_or("").to_string()
    } else {
        h.split(':').next().unwrap_or("").to_string()
    };
    matches!(host.as_str(), "127.0.0.1" | "localhost" | "::1")
}

fn origin_ok(origin: &str) -> bool {
    if origin == "null" {
        return false;
    }
    let rest = origin
        .strip_prefix("http://")
        .or_else(|| origin.strip_prefix("https://"));
    rest.is_some_and(|r| is_local_host(r.split('/').next().unwrap_or("")))
}

/// Constant-time comparison so the token can't be guessed by timing.
fn same(a: &str, b: &str) -> bool {
    a.len() == b.len()
        && a.bytes()
            .zip(b.bytes())
            .fold(0u8, |acc, (x, y)| acc | (x ^ y))
            == 0
}

fn serve(
    conn: TcpStream,
    server: &Arc<Server>,
    token: Option<&str>,
    activity: &Mutex<Activity>,
) -> std::io::Result<()> {
    conn.set_read_timeout(Some(Duration::from_secs(30)))?;
    let mut w = conn.try_clone()?;
    let mut reader = BufReader::new(conn);
    let req = match read_request(&mut reader) {
        Ok(Some(r)) => r,
        Ok(None) => return Ok(()),
        Err(e) => return json_error(&mut w, 400, "Bad Request", &e, &[]),
    };

    // DNS-rebinding protection.
    if !req.headers.get("host").is_some_and(|h| is_local_host(h)) {
        return json_error(
            &mut w,
            403,
            "Forbidden",
            "Host header must be 127.0.0.1 or localhost",
            &[],
        );
    }
    if let Some(o) = req.headers.get("origin")
        && !origin_ok(o)
    {
        return json_error(
            &mut w,
            403,
            "Forbidden",
            "requests from web pages are not allowed",
            &[],
        );
    }
    if req.path != "/mcp" && req.path != "/mcp/" {
        return json_error(&mut w, 404, "Not Found", "the MCP endpoint is /mcp", &[]);
    }
    if let Some(t) = token {
        let ok = req
            .headers
            .get("authorization")
            .and_then(|a| {
                a.strip_prefix("Bearer ")
                    .or_else(|| a.strip_prefix("bearer "))
            })
            .is_some_and(|given| same(given.trim(), t));
        if !ok {
            return json_error(
                &mut w,
                401,
                "Unauthorized",
                "missing or wrong access token (Authorization: Bearer <token>; see Agents ▸ MCP Server in Rust Audacity)",
                &[("WWW-Authenticate", "Bearer".into())],
            );
        }
    }
    match req.method.as_str() {
        "POST" => {}
        "DELETE" => return write_response(&mut w, 200, "OK", &[], b""),
        "OPTIONS" => {
            return write_response(
                &mut w,
                204,
                "No Content",
                &[("Allow", "POST, DELETE".into())],
                b"",
            );
        }
        _ => {
            return write_response(
                &mut w,
                405,
                "Method Not Allowed",
                &[("Allow", "POST, DELETE".into())],
                b"",
            );
        }
    }

    let parsed: Value = match serde_json::from_slice(&req.body) {
        Ok(v) => v,
        Err(e) => {
            let body = super::error(&Value::Null, -32700, &format!("parse error: {e}")).to_string();
            return write_response(
                &mut w,
                400,
                "Bad Request",
                &[("Content-Type", "application/json".into())],
                body.as_bytes(),
            );
        }
    };
    let batch = parsed.is_array();
    let msgs: Vec<Value> = match parsed {
        Value::Array(a) => a,
        v => vec![v],
    };

    {
        let mut a = activity.lock().unwrap_or_else(|e| e.into_inner());
        a.requests += 1;
        a.last_seen = Some(Instant::now());
        for m in &msgs {
            if let Some(name) = m.pointer("/params/clientInfo/name").and_then(Value::as_str) {
                a.last_client = Some(name.to_string());
            }
            if m.get("method").and_then(Value::as_str) == Some("tools/call") {
                a.last_tool = m
                    .pointer("/params/name")
                    .and_then(Value::as_str)
                    .map(str::to_string);
            }
        }
    }

    let has_request = msgs.iter().any(|m| matches!(kind(m), Kind::Request));
    if !has_request {
        for m in &msgs {
            server.handle(m, Arc::new(|_| {}));
        }
        return write_response(&mut w, 202, "Accepted", &[], b"");
    }
    let mut headers = Vec::new();
    if msgs
        .iter()
        .any(|m| m.get("method").and_then(Value::as_str) == Some("initialize"))
    {
        headers.push(("Mcp-Session-Id", random_token()));
    }
    let wants_sse = req
        .headers
        .get("accept")
        .is_some_and(|a| a.contains("text/event-stream"));
    let calls_tool = msgs
        .iter()
        .any(|m| m.get("method").and_then(Value::as_str) == Some("tools/call"));

    if wants_sse && calls_tool {
        // Stream progress notifications, then the result(s).
        let mut head = String::from(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nConnection: close\r\n",
        );
        for (k, v) in &headers {
            head.push_str(&format!("{k}: {v}\r\n"));
        }
        head.push_str("\r\n");
        w.write_all(head.as_bytes())?;
        w.flush()?;
        let sink = Arc::new(Mutex::new(w));
        let send = {
            let sink = sink.clone();
            move |v: &Value| {
                let mut s = sink.lock().unwrap_or_else(|e| e.into_inner());
                let _ = s.write_all(format!("event: message\ndata: {v}\n\n").as_bytes());
                let _ = s.flush();
            }
        };
        let send = Arc::new(send);
        for m in &msgs {
            let s2 = send.clone();
            let notify: Notify = Arc::new(move |n| s2(&n));
            if let Some(r) = server.handle(m, notify) {
                send(&r);
            }
        }
        return Ok(());
    }

    let mut responses = Vec::new();
    for m in &msgs {
        if let Some(r) = server.handle(m, Arc::new(|_| {})) {
            responses.push(r);
        }
    }
    let body = if batch {
        Value::Array(responses)
    } else {
        responses.pop().unwrap_or(Value::Null)
    };
    headers.push(("Content-Type", "application/json".into()));
    write_response(&mut w, 200, "OK", &headers, body.to_string().as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp::Config;

    fn raw(port: u16, req: &str) -> String {
        let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
        s.write_all(req.as_bytes()).unwrap();
        let mut out = String::new();
        s.read_to_string(&mut out).unwrap();
        out
    }

    fn post(port: u16, token: Option<&str>, accept: &str, body: &str) -> String {
        let auth = token
            .map(|t| format!("Authorization: Bearer {t}\r\n"))
            .unwrap_or_default();
        raw(
            port,
            &format!(
                "POST /mcp HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nContent-Type: application/json\r\nAccept: {accept}\r\n{auth}Content-Length: {}\r\n\r\n{body}",
                body.len()
            ),
        )
    }

    #[test]
    fn streamable_http_basics_and_security() {
        let server = Server::new(Config::default());
        let http = HttpServer::start(
            server,
            HttpOptions {
                port: 0,
                token: Some("s3cret".into()),
            },
        )
        .unwrap();
        let port = http.port;
        let both = "application/json, text/event-stream";
        let init = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"test-client","version":"1"}}}"#;

        // No / wrong token.
        assert!(post(port, None, both, init).starts_with("HTTP/1.1 401"));
        assert!(post(port, Some("nope"), both, init).starts_with("HTTP/1.1 401"));

        // Initialize: JSON answer with a session id.
        let r = post(port, Some("s3cret"), both, init);
        assert!(r.starts_with("HTTP/1.1 200"), "{r}");
        assert!(r.contains("Mcp-Session-Id: "));
        assert!(r.contains("\"protocolVersion\":\"2025-06-18\""));
        assert_eq!(http.activity().last_client.as_deref(), Some("test-client"));

        // Notification → 202.
        let r = post(
            port,
            Some("s3cret"),
            both,
            r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
        );
        assert!(r.starts_with("HTTP/1.1 202"), "{r}");

        // tools/list as JSON.
        let r = post(
            port,
            Some("s3cret"),
            "application/json",
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#,
        );
        let body: Value = serde_json::from_str(r.split("\r\n\r\n").nth(1).unwrap()).unwrap();
        assert_eq!(
            body["result"]["tools"].as_array().unwrap().len(),
            crate::mcp::tools::definitions().len()
        );

        // A tool call with SSE: one `message` event carrying the result.
        let r = post(
            port,
            Some("s3cret"),
            both,
            r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"list_effects","arguments":{},"_meta":{"progressToken":"p"}}}"#,
        );
        assert!(r.contains("Content-Type: text/event-stream"), "{r}");
        let data = r.lines().find(|l| l.starts_with("data: ")).unwrap();
        let v: Value = serde_json::from_str(&data[6..]).unwrap();
        assert_eq!(v["id"], 3);
        assert_eq!(v["result"]["isError"], false);

        // Batch.
        let r = post(
            port,
            Some("s3cret"),
            "application/json",
            r#"[{"jsonrpc":"2.0","id":4,"method":"ping"},{"jsonrpc":"2.0","id":5,"method":"ping"}]"#,
        );
        let body: Value = serde_json::from_str(r.split("\r\n\r\n").nth(1).unwrap()).unwrap();
        assert_eq!(body.as_array().unwrap().len(), 2);

        // DNS rebinding: foreign Host or Origin is refused.
        let evil = raw(
            port,
            &format!(
                "POST /mcp HTTP/1.1\r\nHost: evil.example:{port}\r\nAuthorization: Bearer s3cret\r\nContent-Length: 2\r\n\r\n{{}}"
            ),
        );
        assert!(evil.starts_with("HTTP/1.1 403"), "{evil}");
        let evil = raw(
            port,
            &format!(
                "POST /mcp HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nOrigin: https://evil.example\r\nAuthorization: Bearer s3cret\r\nContent-Length: 2\r\n\r\n{{}}"
            ),
        );
        assert!(evil.starts_with("HTTP/1.1 403"), "{evil}");

        // GET → 405, wrong path → 404.
        let r = raw(
            port,
            &format!(
                "GET /mcp HTTP/1.1\r\nHost: localhost:{port}\r\nAuthorization: Bearer s3cret\r\n\r\n"
            ),
        );
        assert!(r.starts_with("HTTP/1.1 405"), "{r}");
        let r = raw(
            port,
            &format!("GET /other HTTP/1.1\r\nHost: localhost:{port}\r\n\r\n"),
        );
        assert!(r.starts_with("HTTP/1.1 404"), "{r}");

        assert!(
            is_local_host("[::1]:80")
                && is_local_host("LOCALHOST")
                && !is_local_host("127.0.0.1.evil.com")
        );
        assert!(
            origin_ok("http://localhost:3000")
                && !origin_ok("null")
                && !origin_ok("http://evil.com")
        );
        drop(http);
    }
}
