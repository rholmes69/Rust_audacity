//! Agents ▸ Assistant: chat with a local Ollama model that can use every Rust Audacity
//! tool, including the app_* tools that edit the open project. Runs in the background;
//! each tool call shows up in the conversation.

use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{Receiver, channel},
    },
    time::Duration,
};

use eframe::egui::{self, Color32, RichText};
use serde_json::{Value, json};

use super::AudacityApp;
use crate::{
    mcp::{Config, Server, tools},
    ollama::{self, Ollama, TurnEvent, TurnOptions},
};

const KEY_URL: &str = "assistant_ollama_url";
const KEY_MODEL: &str = "assistant_model";

enum Item {
    User(String),
    Assistant(String),
    Note(String),
    Tool {
        name: String,
        args: String,
        result: Option<(String, bool)>,
    },
    Error(String),
}

enum Msg {
    Event(TurnEvent),
    Done(Result<String, String>, Vec<Value>),
    Models(Result<Vec<String>, String>),
}

pub struct ChatState {
    pub open: bool,
    url: String,
    model: String,
    models: Vec<String>,
    models_error: Option<String>,
    input: String,
    items: Vec<Item>,
    /// Conversation as sent to Ollama (system prompt first).
    history: Vec<Value>,
    rx: Option<Receiver<Msg>>,
    cancel: Arc<AtomicBool>,
    think: bool,
    asked_models: bool,
}

impl ChatState {
    pub fn load(storage: Option<&dyn eframe::Storage>) -> Self {
        Self {
            open: false,
            url: storage
                .and_then(|s| eframe::get_value(s, KEY_URL))
                .unwrap_or_else(ollama::default_url),
            model: storage
                .and_then(|s| eframe::get_value(s, KEY_MODEL))
                .unwrap_or_else(|| "qwen3:8b".into()),
            models: Vec::new(),
            models_error: None,
            input: String::new(),
            items: Vec::new(),
            history: Vec::new(),
            rx: None,
            cancel: Arc::default(),
            think: false,
            asked_models: false,
        }
    }

    pub fn save(&self, storage: &mut dyn eframe::Storage) {
        eframe::set_value(storage, KEY_URL, &self.url);
        eframe::set_value(storage, KEY_MODEL, &self.model);
    }

    pub fn busy(&self) -> bool {
        self.rx.is_some()
    }
}

const SYSTEM: &str = "You are an audio-editing assistant inside Rust Audacity, a desktop audio editor. \
The user is looking at the app while you work. Tools named app_* act on the project open in the window \
(each change is one undo step the user can reverse with Ctrl+Z); the other tools work on audio files on disk.\n\
Guidelines:\n\
- Start with app_status to see the tracks, selection and length.\n\
- To change part of the audio, select it first with app_select (seconds), then edit or apply effects.\n\
- Effects are given like {\"effect\": \"compressor\", \"params\": {\"ratio\": 3}}; list_effects shows ids and parameters.\n\
- Prefer non-destructive live effects (app_live_fx) when the user wants to try a sound.\n\
- Keep answers short; say what you changed and how to undo it.";

impl AudacityApp {
    fn chat_config(&mut self) -> Option<Config> {
        // The app_* tools reach this window through the agent bridge.
        if !self.bridge.enabled {
            self.bridge.enabled = true;
        }
        let bridge = self.bridge.info()?;
        Some(Config {
            root: self.mcp.root.clone(),
            demucs_command: self.stems.demucs_command().to_string(),
            python: self.transcript.python().to_string(),
            bridge: Some(bridge),
        })
    }

    fn fetch_models(&mut self) {
        let url = self.chat.url.clone();
        let (tx, rx) = channel();
        std::thread::spawn(move || {
            let _ = tx.send(Msg::Models(Ollama::new(&url).models()));
        });
        self.chat.rx = Some(rx);
        self.chat.asked_models = true;
    }

    fn send_chat(&mut self) {
        let text = self.chat.input.trim().to_string();
        if text.is_empty() || self.chat.busy() {
            return;
        }
        let Some(cfg) = self.chat_config() else {
            self.chat.items.push(Item::Error(
                "The agent bridge is starting; try again in a moment.".into(),
            ));
            return;
        };
        self.chat.input.clear();
        self.chat.items.push(Item::User(text.clone()));
        if self.chat.history.is_empty() {
            self.chat
                .history
                .push(json!({ "role": "system", "content": SYSTEM }));
        }
        self.chat
            .history
            .push(json!({ "role": "user", "content": text }));
        let mut history = self.chat.history.clone();
        let opts = TurnOptions {
            model: self.chat.model.clone(),
            ctx: 16384,
            think: self.chat.think,
            max_steps: 16,
        };
        let url = self.chat.url.clone();
        self.chat.cancel = Arc::default();
        let cancel = self.chat.cancel.clone();
        let (tx, rx) = channel();
        self.chat.rx = Some(rx);
        let ctx = self.egui_ctx.clone();
        std::thread::Builder::new()
            .name("assistant".into())
            .spawn(move || {
                let server = Server::new(cfg);
                let tools = ollama::to_ollama_tools(&tools::definitions());
                let ollama = Ollama::new(&url);
                let mut n = 0u64;
                let mut call_tool = |name: &str, args: &Value| -> (String, bool) {
                    n += 1;
                    let req = json!({ "jsonrpc": "2.0", "id": n, "method": "tools/call", "params": { "name": name, "arguments": args } });
                    let Some(resp) = server.handle(&req, Arc::new(|_| {})) else {
                        return ("cancelled".into(), true);
                    };
                    if let Some(e) = resp.get("error") {
                        return (e["message"].as_str().unwrap_or("error").to_string(), true);
                    }
                    let res = &resp["result"];
                    let text: String = res["content"]
                        .as_array()
                        .cloned()
                        .unwrap_or_default()
                        .iter()
                        .filter_map(|c| c["text"].as_str().map(String::from))
                        .collect::<Vec<_>>()
                        .join("\n");
                    (text, res["isError"].as_bool().unwrap_or(false))
                };
                let tx2 = tx.clone();
                let ctx2 = ctx.clone();
                let mut on_event = |e: TurnEvent| {
                    let _ = tx2.send(Msg::Event(e));
                    ctx2.request_repaint();
                };
                let r = ollama::run_turn(&ollama, &opts, &tools, &mut history, &mut call_tool, &mut on_event, &cancel);
                let _ = tx.send(Msg::Done(r, history));
                ctx.request_repaint();
            })
            .expect("spawn assistant thread");
    }

    pub(super) fn poll_chat(&mut self) {
        let Some(rx) = &self.chat.rx else { return };
        self.egui_ctx
            .request_repaint_after(Duration::from_millis(250));
        let mut done = false;
        while let Ok(m) = rx.try_recv() {
            match m {
                Msg::Event(TurnEvent::Note(t)) => self.chat.items.push(Item::Note(t)),
                Msg::Event(TurnEvent::ToolStart { name, args }) => {
                    self.chat.items.push(Item::Tool {
                        name,
                        args: ollama::truncate(&args.to_string(), 300),
                        result: None,
                    })
                }
                Msg::Event(TurnEvent::ToolDone { text, is_error, .. }) => {
                    if let Some(Item::Tool { result, .. }) = self
                        .chat
                        .items
                        .iter_mut()
                        .rev()
                        .find(|i| matches!(i, Item::Tool { result: None, .. }))
                    {
                        *result = Some((text, is_error));
                    }
                }
                Msg::Done(r, history) => {
                    match r {
                        Ok(answer) => {
                            self.chat.history = history;
                            self.chat
                                .items
                                .push(Item::Assistant(answer.trim().to_string()));
                        }
                        Err(e) => {
                            // Keep what happened so far, but don't leave a dangling user turn.
                            self.chat.history = history;
                            self.chat.items.push(Item::Error(e));
                        }
                    }
                    done = true;
                }
                Msg::Models(r) => {
                    match r {
                        Ok(m) => {
                            self.chat.models_error = None;
                            if !m.is_empty() && !m.contains(&self.chat.model) {
                                self.chat.model = m[0].clone();
                            }
                            self.chat.models = m;
                        }
                        Err(e) => self.chat.models_error = Some(e),
                    }
                    done = true;
                }
            }
        }
        if done {
            self.chat.rx = None;
        }
    }

    pub(super) fn chat_panel(&mut self, ui: &mut egui::Ui) {
        if !self.chat.asked_models && !self.chat.busy() {
            self.fetch_models();
        }
        ui.horizontal(|ui| {
            ui.heading("Assistant");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.small_button("✕").on_hover_text("Close").clicked() {
                    self.chat.open = false;
                }
                if ui
                    .add_enabled(
                        !self.chat.busy() && !self.chat.items.is_empty(),
                        egui::Button::new("New chat").small(),
                    )
                    .clicked()
                {
                    self.chat.items.clear();
                    self.chat.history.clear();
                }
            });
        });
        ui.horizontal(|ui| {
            ui.label("Model:");
            let busy = self.chat.busy();
            egui::ComboBox::from_id_salt("chat_model")
                .selected_text(self.chat.model.clone())
                .width(170.0)
                .show_ui(ui, |ui| {
                    for m in self.chat.models.clone() {
                        ui.selectable_value(&mut self.chat.model, m.clone(), m);
                    }
                });
            if ui
                .add_enabled(!busy, egui::Button::new("⟳").small())
                .on_hover_text("Reload installed models")
                .clicked()
            {
                self.fetch_models();
            }
            ui.checkbox(&mut self.chat.think, "Think")
                .on_hover_text("Let reasoning models think first (slower)");
        });
        ui.collapsing("Ollama", |ui| {
            ui.horizontal(|ui| {
                ui.label("Address:");
                ui.text_edit_singleline(&mut self.chat.url);
            });
            ui.label(
                RichText::new("Install Ollama from ollama.com, then e.g. `ollama pull qwen3:8b`. Use a model with tool support.")
                    .small()
                    .weak(),
            );
        });
        if let Some(e) = &self.chat.models_error {
            ui.colored_label(Color32::from_rgb(235, 150, 120), e);
        }
        ui.separator();
        let input_h = 64.0;
        let avail = ui.available_height() - input_h - 12.0;
        egui::ScrollArea::vertical()
            .max_height(avail.max(80.0))
            .stick_to_bottom(true)
            .auto_shrink([false, false])
            .show(ui, |ui| {
                if self.chat.items.is_empty() {
                    ui.label(
                        RichText::new(
                            "Ask for edits in plain words, for example:\n\
                             • “Remove the hum and level the voice to −16 LUFS”\n\
                             • “Cut the silence longer than a second”\n\
                             • “Add a gentle reverb to track 2 (live)”\n\
                             • “Transcribe it and delete the filler words”",
                        )
                        .weak(),
                    );
                }
                for item in &self.chat.items {
                    match item {
                        Item::User(t) => {
                            ui.add_space(4.0);
                            ui.label(RichText::new(t).strong());
                        }
                        Item::Assistant(t) => {
                            ui.label(t);
                        }
                        Item::Note(t) => {
                            ui.label(RichText::new(t).italics().weak());
                        }
                        Item::Tool { name, args, result } => {
                            let (icon, col) = match result {
                                None => ("⏳", Color32::from_rgb(140, 170, 220)),
                                Some((_, false)) => ("✓", Color32::from_rgb(110, 200, 120)),
                                Some((_, true)) => ("✗", Color32::from_rgb(235, 110, 90)),
                            };
                            let header = RichText::new(format!("{icon} {name}"))
                                .monospace()
                                .small()
                                .color(col);
                            egui::CollapsingHeader::new(header)
                                .id_salt((name, args.as_str()))
                                .show(ui, |ui| {
                                    ui.label(RichText::new(args).monospace().small().weak());
                                    if let Some((r, _)) = result {
                                        ui.label(RichText::new(ollama::truncate(r, 1500)).small());
                                    }
                                });
                        }
                        Item::Error(e) => {
                            ui.colored_label(Color32::from_rgb(235, 150, 120), e);
                        }
                    }
                }
                if self.chat.busy() && !self.chat.items.is_empty() {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label(RichText::new("Working…").weak());
                    });
                }
            });
        ui.separator();
        ui.horizontal(|ui| {
            let busy = self.chat.busy();
            let r = ui.add_sized(
                [ui.available_width() - 64.0, input_h - 8.0],
                egui::TextEdit::multiline(&mut self.chat.input)
                    .hint_text("Ask the assistant… (Enter to send)"),
            );
            let enter = r.has_focus()
                && ui.input(|i| i.key_pressed(egui::Key::Enter) && !i.modifiers.shift);
            if busy {
                if ui.button("■ Stop").clicked() {
                    self.chat.cancel.store(true, Ordering::Relaxed);
                }
            } else if ui.button("Send").clicked() || enter {
                // Enter adds a newline in a multiline box; drop it.
                let t = self.chat.input.trim_end_matches('\n').to_string();
                self.chat.input = t;
                self.send_chat();
            }
        });
    }
}
