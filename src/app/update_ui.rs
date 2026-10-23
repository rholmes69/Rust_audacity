//! Help ▸ Check for Updates (and a quiet check at startup, at most once a day).

use std::{
    sync::mpsc::{Receiver, channel},
    time::Duration,
};

use eframe::egui::{self, RichText};
use serde_json::Value;

use super::AudacityApp;

const KEY_AUTO: &str = "update_check_auto";
const KEY_LAST: &str = "update_check_last";
const API: &str = "https://api.github.com/repos/rholmes69/Rust_audacity/releases/latest";
pub const RELEASES: &str = "https://github.com/rholmes69/Rust_audacity/releases";

#[derive(Clone, Debug)]
pub struct Release {
    pub version: String,
    pub url: String,
    pub notes: String,
}

pub struct UpdateState {
    pub auto: bool,
    last_check: u64,
    rx: Option<Receiver<Result<Release, String>>>,
    /// Show the result window (a manual check, or a startup check that found something).
    window: bool,
    manual: bool,
    result: Option<Result<Release, String>>,
}

impl UpdateState {
    pub fn load(storage: Option<&dyn eframe::Storage>) -> Self {
        Self {
            auto: storage
                .and_then(|s| eframe::get_value(s, KEY_AUTO))
                .unwrap_or(true),
            last_check: storage
                .and_then(|s| eframe::get_value(s, KEY_LAST))
                .unwrap_or(0),
            rx: None,
            window: false,
            manual: false,
            result: None,
        }
    }
    pub fn save(&self, storage: &mut dyn eframe::Storage) {
        eframe::set_value(storage, KEY_AUTO, &self.auto);
        eframe::set_value(storage, KEY_LAST, &self.last_check);
    }
}

/// "v0.10.2" → (0, 10, 2).
pub fn parse_version(s: &str) -> Option<(u64, u64, u64)> {
    let s = s.trim().trim_start_matches(['v', 'V']);
    let core = s.split(['-', '+']).next()?;
    let mut it = core.split('.').map(|p| p.parse::<u64>().ok());
    Some((
        it.next()??,
        it.next().flatten().unwrap_or(0),
        it.next().flatten().unwrap_or(0),
    ))
}

pub fn is_newer(candidate: &str, current: &str) -> bool {
    match (parse_version(candidate), parse_version(current)) {
        (Some(a), Some(b)) => a > b,
        _ => false,
    }
}

fn fetch() -> Result<Release, String> {
    // Use the operating system's certificates, so networks that inspect HTTPS (offices,
    // schools) work too.
    let tls = ureq::tls::TlsConfig::builder()
        .root_certs(ureq::tls::RootCerts::PlatformVerifier)
        .build();
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(15)))
        .http_status_as_error(false)
        .tls_config(tls)
        .build()
        .into();
    let mut resp = agent
        .get(API)
        .header(
            "User-Agent",
            &format!("RustAudacity/{}", env!("CARGO_PKG_VERSION")),
        )
        .header("Accept", "application/vnd.github+json")
        .call()
        .map_err(|e| format!("Couldn't reach GitHub: {e}"))?;
    if resp.status() == 404 {
        return Err("No releases have been published yet.".into());
    }
    if !resp.status().is_success() {
        return Err(format!("GitHub answered {}", resp.status()));
    }
    let v: Value = resp.body_mut().read_json().map_err(|e| e.to_string())?;
    let tag = v["tag_name"]
        .as_str()
        .ok_or("unexpected answer from GitHub")?
        .to_string();
    // Prefer the installer, then the zip, then the release page.
    let assets = v["assets"].as_array().cloned().unwrap_or_default();
    let pick = |suffix: &str| {
        assets
            .iter()
            .find(|a| a["name"].as_str().is_some_and(|n| n.ends_with(suffix)))
            .and_then(|a| a["browser_download_url"].as_str().map(String::from))
    };
    let url = pick("-setup.exe")
        .or_else(|| pick(".zip"))
        .or_else(|| v["html_url"].as_str().map(String::from))
        .unwrap_or_else(|| RELEASES.into());
    Ok(Release {
        version: tag.trim_start_matches('v').to_string(),
        url,
        notes: v["body"]
            .as_str()
            .unwrap_or("")
            .chars()
            .take(1200)
            .collect(),
    })
}

impl AudacityApp {
    pub(super) fn check_for_updates(&mut self, manual: bool) {
        if self.updates.rx.is_some() {
            return;
        }
        let (tx, rx) = channel();
        let ctx = self.egui_ctx.clone();
        std::thread::spawn(move || {
            let _ = tx.send(fetch());
            ctx.request_repaint();
        });
        self.updates.rx = Some(rx);
        self.updates.manual = manual;
        if manual {
            self.updates.window = true;
            self.updates.result = None;
        }
        self.updates.last_check = crate::recovery::now();
    }

    /// Startup check (once a day, if enabled) and collecting results.
    pub(super) fn poll_updates(&mut self) {
        if self.updates.auto
            && self.updates.rx.is_none()
            && self.updates.result.is_none()
            && crate::recovery::now().saturating_sub(self.updates.last_check) > 24 * 3600
        {
            self.check_for_updates(false);
        }
        let Some(rx) = &self.updates.rx else { return };
        let Ok(r) = rx.try_recv() else { return };
        self.updates.rx = None;
        let newer = matches!(&r, Ok(rel) if is_newer(&rel.version, env!("CARGO_PKG_VERSION")));
        if self.updates.manual || newer {
            self.updates.window = true;
        }
        if newer && let Ok(rel) = &r {
            self.status = format!(
                "Rust Audacity {} is available (Help ▸ Check for Updates)",
                rel.version
            );
        }
        self.updates.result = Some(r);
    }

    pub(super) fn update_window(&mut self, ctx: &egui::Context) {
        if !self.updates.window {
            return;
        }
        let mut open = true;
        let mut download = None;
        egui::Window::new("Check for Updates")
            .collapsible(false)
            .resizable(false)
            .default_width(420.0)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .open(&mut open)
            .show(ctx, |ui| {
                let current = env!("CARGO_PKG_VERSION");
                match &self.updates.result {
                    None => {
                        ui.horizontal(|ui| {
                            ui.spinner();
                            ui.label("Checking GitHub for a newer version…");
                        });
                    }
                    Some(Err(e)) => {
                        ui.label(format!("Couldn't check for updates: {e}"));
                        if ui.link("Open the releases page").clicked() {
                            download = Some(RELEASES.to_string());
                        }
                    }
                    Some(Ok(rel)) if is_newer(&rel.version, current) => {
                        ui.heading(format!("Rust Audacity {} is available", rel.version));
                        ui.label(format!("You have {current}."));
                        if !rel.notes.trim().is_empty() {
                            egui::ScrollArea::vertical()
                                .max_height(180.0)
                                .show(ui, |ui| {
                                    ui.label(RichText::new(rel.notes.trim()).small());
                                });
                        }
                        ui.horizontal(|ui| {
                            let label = if rel.url.ends_with(".exe") {
                                "⬇ Download installer"
                            } else {
                                "⬇ Download"
                            };
                            if ui.button(label).clicked() {
                                download = Some(rel.url.clone());
                            }
                            if ui.button("Release page").clicked() {
                                download = Some(RELEASES.to_string());
                            }
                        });
                    }
                    Some(Ok(rel)) => {
                        ui.label(format!(
                            "You have the latest version ({current}; newest release {}).",
                            rel.version
                        ));
                    }
                }
                ui.separator();
                ui.checkbox(&mut self.updates.auto, "Check automatically (once a day)");
            });
        if let Some(url) = download {
            ctx.open_url(egui::OpenUrl::new_tab(url));
        }
        if !open {
            self.updates.window = false;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_compare() {
        assert!(is_newer("v0.10.0", "0.9.5"));
        assert!(is_newer("1.0", "0.99.99"));
        assert!(!is_newer("v0.8.0", "0.8.0"));
        assert!(!is_newer("0.7.9", "0.8.0"));
        assert!(!is_newer("nightly", "0.8.0"));
        assert_eq!(parse_version("v2.3.4-beta"), Some((2, 3, 4)));
    }
}
