//! The built-in MCP server (Streamable HTTP on 127.0.0.1) and its settings window
//! (Agents ▸ MCP Server…).

use std::{path::PathBuf, sync::Arc};

use eframe::egui::{self, Color32, RichText};

use super::AudacityApp;
use crate::mcp::{
    Config, Server,
    http::{HttpOptions, HttpServer, random_token},
};

const KEY_ENABLED: &str = "mcp_http_enabled";
const KEY_PORT: &str = "mcp_http_port";
const KEY_TOKEN_ON: &str = "mcp_http_require_token";
const KEY_TOKEN: &str = "mcp_http_token";
const KEY_ROOT: &str = "mcp_http_root";
pub const DEFAULT_PORT: u16 = 8765;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Client {
    ClaudeCode,
    Cursor,
    VsCode,
    ClaudeDesktop,
}

pub struct McpState {
    pub enabled: bool,
    port: u16,
    require_token: bool,
    token: String,
    /// Folder the file tools may use (None = any folder).
    root: Option<PathBuf>,
    server: Option<HttpServer>,
    core: Option<Arc<Server>>,
    /// Settings the running server was started with (restart when they change).
    running_with: Option<(u16, Option<String>)>,
    pub error: Option<String>,
    pub window_open: bool,
    show_token: bool,
    client: Client,
    port_edit: u16,
}

impl McpState {
    pub fn load(storage: Option<&dyn eframe::Storage>) -> Self {
        let get_s = |k: &str| storage.and_then(|s| eframe::get_value::<String>(s, k));
        let env_port = std::env::var("RUST_AUDACITY_MCP_PORT")
            .ok()
            .and_then(|p| p.parse().ok());
        let env_token = std::env::var("RUST_AUDACITY_MCP_TOKEN")
            .ok()
            .filter(|t| !t.is_empty());
        let port = env_port
            .or_else(|| storage.and_then(|s| eframe::get_value::<u16>(s, KEY_PORT)))
            .unwrap_or(DEFAULT_PORT);
        let token = env_token
            .or_else(|| get_s(KEY_TOKEN).filter(|t| t.len() >= 16))
            .unwrap_or_else(random_token);
        Self {
            enabled: storage
                .and_then(|s| eframe::get_value(s, KEY_ENABLED))
                .unwrap_or(true),
            port,
            require_token: storage
                .and_then(|s| eframe::get_value(s, KEY_TOKEN_ON))
                .unwrap_or(true),
            token,
            root: get_s(KEY_ROOT).filter(|s| !s.is_empty()).map(PathBuf::from),
            server: None,
            core: None,
            running_with: None,
            error: None,
            window_open: false,
            show_token: false,
            client: Client::ClaudeCode,
            port_edit: port,
        }
    }

    pub fn save(&self, storage: &mut dyn eframe::Storage) {
        eframe::set_value(storage, KEY_ENABLED, &self.enabled);
        eframe::set_value(storage, KEY_PORT, &self.port);
        eframe::set_value(storage, KEY_TOKEN_ON, &self.require_token);
        eframe::set_value(storage, KEY_TOKEN, &self.token);
        eframe::set_value(
            storage,
            KEY_ROOT,
            &self
                .root
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_default(),
        );
    }

    pub fn url(&self) -> String {
        format!("http://127.0.0.1:{}/mcp", self.port)
    }

    pub fn running(&self) -> bool {
        self.server.is_some()
    }

    fn wanted(&self) -> (u16, Option<String>) {
        (self.port, self.require_token.then(|| self.token.clone()))
    }

    pub fn shutdown(&mut self) {
        self.server = None;
        self.running_with = None;
    }

    /// Status-bar text: running state plus the latest client activity.
    pub fn status_text(&self) -> Option<(String, bool)> {
        let s = self.server.as_ref()?;
        let a = s.activity();
        let recent = a.last_seen.is_some_and(|t| t.elapsed().as_secs_f32() < 5.0);
        let text = match (recent, &a.last_client, &a.last_tool) {
            (true, Some(c), Some(t)) => format!("MCP ● {c}: {t}"),
            (true, Some(c), None) => format!("MCP ● {c}"),
            _ => format!("MCP ● :{}", self.port),
        };
        Some((text, recent))
    }

    fn snippet(&self) -> (String, &'static str) {
        let url = self.url();
        let auth = format!("Bearer {}", self.token);
        let tok = self.require_token;
        match self.client {
            Client::ClaudeCode => (
                if tok {
                    format!(
                        "claude mcp add --transport http --scope user rust-audacity {url} --header \"Authorization: {auth}\""
                    )
                } else {
                    format!("claude mcp add --transport http --scope user rust-audacity {url}")
                },
                "Run this once in a terminal (PowerShell or cmd).",
            ),
            Client::Cursor => {
                let mut v =
                    serde_json::json!({ "mcpServers": { "rust-audacity": { "url": url } } });
                if tok {
                    v["mcpServers"]["rust-audacity"]["headers"] =
                        serde_json::json!({ "Authorization": auth });
                }
                (
                    serde_json::to_string_pretty(&v).unwrap_or_default(),
                    "Add to %USERPROFILE%\\.cursor\\mcp.json (Windsurf: ~/.codeium/windsurf/mcp_config.json, using \"serverUrl\").",
                )
            }
            Client::VsCode => {
                let mut v = serde_json::json!({ "servers": { "rust-audacity": { "type": "http", "url": url } } });
                if tok {
                    v["servers"]["rust-audacity"]["headers"] =
                        serde_json::json!({ "Authorization": auth });
                }
                (
                    serde_json::to_string_pretty(&v).unwrap_or_default(),
                    "Add to .vscode/mcp.json in your workspace (or the user mcp.json), then start the server from the MCP view.",
                )
            }
            Client::ClaudeDesktop => {
                let mut v = serde_json::json!({ "mcpServers": { "rust-audacity": {
                    "command": "npx",
                    "args": ["-y", "mcp-remote", url, "--allow-http"],
                } } });
                if tok {
                    let args = v["mcpServers"]["rust-audacity"]["args"]
                        .as_array_mut()
                        .unwrap();
                    args.push("--header".into());
                    args.push("Authorization:${AUTH_HEADER}".into());
                    v["mcpServers"]["rust-audacity"]["env"] =
                        serde_json::json!({ "AUTH_HEADER": auth });
                }
                (
                    serde_json::to_string_pretty(&v).unwrap_or_default(),
                    "Claude Desktop only starts local servers as programs. This uses the mcp-remote bridge \
                     (needs Node.js). Simpler alternative: point Claude Desktop at rust_audacity_mcp.exe (see MCP.md).",
                )
            }
        }
    }
}

impl AudacityApp {
    /// Start, stop or restart the server to match the settings. Called every frame.
    pub(super) fn sync_mcp(&mut self) {
        let m = &mut self.mcp;
        if !m.enabled {
            if m.server.is_some() {
                m.shutdown();
            }
            return;
        }
        if m.server.is_some() && m.running_with.as_ref() != Some(&m.wanted()) {
            m.shutdown();
        }
        if m.server.is_some() || m.error.is_some() {
            // Keep live settings current without restarting.
            if let Some(core) = &m.core
                && let Ok(mut cfg) = core.cfg.lock()
            {
                cfg.root.clone_from(&m.root);
                cfg.python = self.transcript.python().to_string();
                cfg.demucs_command = self.stems.demucs_command().to_string();
                cfg.bridge = self.bridge.info();
            }
            return;
        }
        // The app_* tools reach this window through the agent bridge.
        if !self.bridge.enabled {
            self.bridge.enabled = true;
        }
        let Some(bridge) = self.bridge.info() else {
            return; // bridge starts this frame; try again next frame
        };
        let cfg = Config {
            root: m.root.clone(),
            demucs_command: self.stems.demucs_command().to_string(),
            python: self.transcript.python().to_string(),
            bridge: Some(bridge),
        };
        let core = Server::new(cfg);
        let (port, token) = m.wanted();
        match HttpServer::start(
            core.clone(),
            HttpOptions {
                port,
                token: token.clone(),
            },
        ) {
            Ok(s) => {
                m.server = Some(s);
                m.core = Some(core);
                m.running_with = Some((port, token));
                m.error = None;
            }
            Err(e) => {
                m.error = Some(if e.kind() == std::io::ErrorKind::AddrInUse {
                    format!(
                        "Port {port} is already in use (another copy of Rust Audacity?). Pick another port."
                    )
                } else {
                    format!("The MCP server could not start: {e}")
                });
            }
        }
    }

    pub(super) fn mcp_window(&mut self, ctx: &egui::Context) {
        if !self.mcp.window_open {
            return;
        }
        let mut open = true;
        egui::Window::new("MCP Server")
            .collapsible(false)
            .resizable(false)
            .default_width(520.0)
            .open(&mut open)
            .show(ctx, |ui| {
                ui.label(
                    RichText::new(
                        "Rust Audacity can serve its AI-agent tools itself. Agents such as Claude Code, Cursor \
                         or VS Code connect to this window by URL and can edit what you have open (app_* tools) \
                         or work on audio files.",
                    )
                    .weak(),
                );
                ui.add_space(6.0);
                let m = &mut self.mcp;
                if ui.checkbox(&mut m.enabled, "Run the built-in MCP server").changed() {
                    m.error = None;
                }
                ui.horizontal(|ui| {
                    if let Some(e) = &m.error {
                        ui.colored_label(Color32::from_rgb(235, 120, 90), e);
                    } else if m.server.is_some() {
                        ui.colored_label(Color32::from_rgb(110, 210, 120), "● Running at");
                        ui.monospace(m.url());
                        if ui.small_button("Copy").clicked() {
                            ui.ctx().copy_text(m.url());
                        }
                    } else if m.enabled {
                        ui.label("Starting…");
                    } else {
                        ui.label(RichText::new("Off").weak());
                    }
                });
                ui.add_space(4.0);
                egui::Grid::new("mcp_grid").num_columns(2).spacing([12.0, 6.0]).show(ui, |ui| {
                    ui.label("Port:");
                    ui.horizontal(|ui| {
                        ui.add(egui::DragValue::new(&mut m.port_edit).range(1024..=65535));
                        if m.port_edit != m.port && ui.button("Apply").clicked() {
                            m.port = m.port_edit;
                            m.error = None;
                        }
                        if m.port != DEFAULT_PORT
                            && ui.small_button("Default").clicked()
                        {
                            m.port = DEFAULT_PORT;
                            m.port_edit = DEFAULT_PORT;
                            m.error = None;
                        }
                    });
                    ui.end_row();
                    ui.label("Access token:");
                    ui.vertical(|ui| {
                        ui.checkbox(&mut m.require_token, "Require a token (recommended)");
                        if m.require_token {
                            ui.horizontal(|ui| {
                                let shown = if m.show_token {
                                    m.token.clone()
                                } else {
                                    format!("••••••••{}", &m.token[m.token.len().saturating_sub(4)..])
                                };
                                ui.monospace(shown);
                                if ui.small_button(if m.show_token { "Hide" } else { "Show" }).clicked() {
                                    m.show_token ^= true;
                                }
                                if ui.small_button("Copy").clicked() {
                                    ui.ctx().copy_text(m.token.clone());
                                }
                                if ui
                                    .small_button("New token")
                                    .on_hover_text("Agents set up with the old token stop working")
                                    .clicked()
                                {
                                    m.token = random_token();
                                }
                            });
                        }
                    });
                    ui.end_row();
                    ui.label("File access:");
                    ui.vertical(|ui| {
                        ui.label(match &m.root {
                            Some(r) => format!("Only inside {}", r.display()),
                            None => "Any folder on this computer".into(),
                        });
                        ui.horizontal(|ui| {
                            if ui.button("Limit to folder…").clicked()
                                && let Some(d) = rfd::FileDialog::new().pick_folder()
                            {
                                m.root = std::fs::canonicalize(&d).ok().or(Some(d));
                            }
                            if m.root.is_some() && ui.button("Allow any folder").clicked() {
                                m.root = None;
                            }
                        });
                    });
                    ui.end_row();
                });
                ui.label(
                    RichText::new(
                        "Only programs on this computer can connect (127.0.0.1). Web pages are blocked.",
                    )
                    .small()
                    .weak(),
                );
                ui.separator();
                ui.strong("Connect an agent");
                ui.horizontal(|ui| {
                    for (c, label) in [
                        (Client::ClaudeCode, "Claude Code"),
                        (Client::Cursor, "Cursor / Windsurf"),
                        (Client::VsCode, "VS Code"),
                        (Client::ClaudeDesktop, "Claude Desktop"),
                    ] {
                        ui.selectable_value(&mut m.client, c, label);
                    }
                });
                let (text, hint) = m.snippet();
                ui.label(RichText::new(hint).small());
                // The box masks the token unless "Show" is on; Copy always copies the real thing.
                let mut shown = if m.show_token || !m.require_token {
                    text.clone()
                } else {
                    text.replace(&m.token, "•••••••• (hidden – Copy includes it)")
                };
                ui.add(
                    egui::TextEdit::multiline(&mut shown)
                        .code_editor()
                        .desired_rows(if text.lines().count() > 1 { 7 } else { 2 })
                        .desired_width(f32::INFINITY),
                );
                if ui.button("📋 Copy").clicked() {
                    ui.ctx().copy_text(text);
                }
                if let Some(s) = &m.server {
                    let a = s.activity();
                    if a.requests > 0 {
                        ui.separator();
                        ui.label(
                            RichText::new(format!(
                                "{} request(s) so far{}{}",
                                a.requests,
                                a.last_client.as_ref().map(|c| format!(" · last client: {c}")).unwrap_or_default(),
                                a.last_tool.as_ref().map(|t| format!(" · last tool: {t}")).unwrap_or_default(),
                            ))
                            .small(),
                        );
                    }
                }
            });
        if !open {
            self.mcp.window_open = false;
        }
    }
}
