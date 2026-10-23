//! Third-party audio plugins (VST3 and CLAP effects).
//!
//! A plugin is loaded on the main thread as a [`LoadedPlugin`], which owns its parameters,
//! state and editor window. Its audio processing part ([`SharedProcess`]) is shared with the
//! audio thread, which locks it (with `try_lock`, so the audio never waits) for each block.
//!
//! Scanning loads every plugin file, which can crash on a broken plugin, so the app scans
//! in a child process (`rust_audacity --scan-plugin <file>`).

pub mod clap;
pub mod vst3;
#[cfg(any(windows, wincheck))]
mod win_editor;

use std::{
    any::Any,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use anyhow::Result;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum PluginFormat {
    Vst3,
    Clap,
}

impl PluginFormat {
    pub fn label(self) -> &'static str {
        match self {
            PluginFormat::Vst3 => "VST3",
            PluginFormat::Clap => "CLAP",
        }
    }
    pub fn extension(self) -> &'static str {
        match self {
            PluginFormat::Vst3 => "vst3",
            PluginFormat::Clap => "clap",
        }
    }
}

/// A plugin found by scanning.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PluginDesc {
    pub format: PluginFormat,
    /// The `.vst3` bundle or `.clap` file.
    pub path: PathBuf,
    /// VST3 class id (hex) or CLAP plugin id.
    pub id: String,
    pub name: String,
    pub vendor: String,
    /// VST3 sub-categories ("Fx|Reverb") or CLAP features ("audio-effect reverb").
    pub category: String,
    pub is_instrument: bool,
}

impl PluginDesc {
    /// Stable key for remembering which plugins are enabled.
    pub fn key(&self) -> String {
        format!(
            "{}:{}:{}",
            self.format.label(),
            self.path.display(),
            self.id
        )
    }
}

#[derive(Clone, Debug)]
pub struct ParamInfo {
    pub id: u32,
    pub name: String,
    pub unit: String,
    pub min: f64,
    pub max: f64,
    pub default: f64,
    pub value: f64,
    /// 0 = continuous, otherwise the number of steps.
    pub steps: u32,
    pub is_bypass: bool,
}

/// Parameter changes waiting for the audio processor: (id, value). VST3 values are
/// normalized (0–1), CLAP values are plain.
#[derive(Clone, Default)]
pub struct ParamQueue(Arc<Mutex<Vec<(u32, f64)>>>);

impl ParamQueue {
    pub fn push(&self, id: u32, value: f64) {
        if let Ok(mut q) = self.0.lock() {
            match q.iter_mut().find(|(i, _)| *i == id) {
                Some(e) => e.1 = value,
                None => q.push((id, value)),
            }
        }
    }
    /// Take everything queued (never blocks: if the UI holds the lock, try next block).
    pub fn drain_into(&self, out: &mut Vec<(u32, f64)>) {
        if let Ok(mut q) = self.0.try_lock() {
            out.append(&mut q);
        }
    }
    pub fn is_empty(&self) -> bool {
        self.0.lock().map_or(true, |q| q.is_empty())
    }
}

/// The audio-thread half of a plugin.
pub trait AudioProcess: Send {
    /// Process planar audio in place (any block length; any channel count).
    fn process(&mut self, chans: &mut [Vec<f32>]);
    /// Deliver queued parameter changes without audio (used while nothing is playing).
    fn flush_params(&mut self) {}
    fn set_rate(&mut self, _rate: f64) {}
    fn as_any_mut(&mut self) -> Option<&mut dyn Any> {
        None
    }
}

pub type SharedProcess = Arc<Mutex<Box<dyn AudioProcess>>>;

/// Leaves audio untouched (what a plugin's shared processor becomes once it's unloaded).
pub struct Bypass;

impl AudioProcess for Bypass {
    fn process(&mut self, _chans: &mut [Vec<f32>]) {}
}

/// A plugin instance, owned by the main thread.
pub enum LoadedPlugin {
    Vst3(Box<vst3::Vst3Plugin>),
    Clap(Box<clap::ClapPlugin>),
}

/// Largest block handed to a plugin at once (bigger blocks are split).
pub const MAX_BLOCK: usize = 4096;

impl LoadedPlugin {
    pub fn load(desc: &PluginDesc, rate: f64) -> Result<Self> {
        Ok(match desc.format {
            PluginFormat::Vst3 => {
                LoadedPlugin::Vst3(Box::new(vst3::Vst3Plugin::load(desc, rate, MAX_BLOCK)?))
            }
            PluginFormat::Clap => {
                LoadedPlugin::Clap(Box::new(clap::ClapPlugin::load(desc, rate, MAX_BLOCK)?))
            }
        })
    }

    pub fn desc(&self) -> &PluginDesc {
        match self {
            LoadedPlugin::Vst3(p) => p.desc(),
            LoadedPlugin::Clap(p) => p.desc(),
        }
    }
    pub fn processor(&self) -> SharedProcess {
        match self {
            LoadedPlugin::Vst3(p) => p.processor(),
            LoadedPlugin::Clap(p) => p.processor(),
        }
    }
    pub fn set_rate(&mut self, rate: f64) -> Result<()> {
        match self {
            LoadedPlugin::Vst3(p) => p.set_rate(rate),
            LoadedPlugin::Clap(p) => p.set_rate(rate),
        }
    }
    pub fn params(&mut self) -> Vec<ParamInfo> {
        match self {
            LoadedPlugin::Vst3(p) => p.params(),
            LoadedPlugin::Clap(p) => p.params(),
        }
    }
    pub fn param_text(&mut self, id: u32, value: f64) -> Option<String> {
        match self {
            LoadedPlugin::Vst3(p) => p.param_text(id, value),
            LoadedPlugin::Clap(p) => p.param_text(id, value),
        }
    }
    pub fn set_param(&mut self, id: u32, value: f64) {
        match self {
            LoadedPlugin::Vst3(p) => p.set_param(id, value),
            LoadedPlugin::Clap(p) => p.set_param(id, value),
        }
    }
    /// Main-thread housekeeping; call regularly (every UI frame).
    pub fn idle(&mut self) {
        match self {
            LoadedPlugin::Vst3(p) => p.idle(),
            LoadedPlugin::Clap(p) => p.idle(),
        }
    }
    pub fn save_state(&mut self) -> Result<Vec<u8>> {
        match self {
            LoadedPlugin::Vst3(p) => p.save_state(),
            LoadedPlugin::Clap(p) => p.save_state(),
        }
    }
    pub fn load_state(&mut self, data: &[u8]) -> Result<()> {
        match self {
            LoadedPlugin::Vst3(p) => p.load_state(data),
            LoadedPlugin::Clap(p) => p.load_state(data),
        }
    }
    pub fn has_editor(&self) -> bool {
        match self {
            LoadedPlugin::Vst3(p) => p.has_editor(),
            LoadedPlugin::Clap(p) => p.has_editor(),
        }
    }
    pub fn open_editor(&mut self, title: &str) -> Result<()> {
        match self {
            LoadedPlugin::Vst3(p) => p.open_editor(title),
            LoadedPlugin::Clap(p) => p.open_editor(title),
        }
    }
    pub fn close_editor(&mut self) {
        match self {
            LoadedPlugin::Vst3(p) => p.close_editor(),
            LoadedPlugin::Clap(p) => p.close_editor(),
        }
    }
    pub fn editor_open(&self) -> bool {
        match self {
            LoadedPlugin::Vst3(p) => p.editor_open(),
            LoadedPlugin::Clap(p) => p.editor_open(),
        }
    }
}

/// Where plugins are normally installed.
pub fn default_dirs() -> Vec<PathBuf> {
    let mut v = Vec::new();
    let env = |k: &str| std::env::var_os(k).map(PathBuf::from);
    if cfg!(any(windows, wincheck)) {
        let common = env("CommonProgramFiles")
            .unwrap_or_else(|| PathBuf::from(r"C:\Program Files\Common Files"));
        v.push(common.join("VST3"));
        v.push(common.join("CLAP"));
        if let Some(l) = env("LOCALAPPDATA") {
            v.push(l.join("Programs").join("Common").join("VST3"));
            v.push(l.join("Programs").join("Common").join("CLAP"));
        }
    } else if cfg!(target_os = "macos") {
        v.push(PathBuf::from("/Library/Audio/Plug-Ins/VST3"));
        v.push(PathBuf::from("/Library/Audio/Plug-Ins/CLAP"));
        if let Some(h) = env("HOME") {
            v.push(h.join("Library/Audio/Plug-Ins/VST3"));
            v.push(h.join("Library/Audio/Plug-Ins/CLAP"));
        }
    } else {
        if let Some(h) = env("HOME") {
            v.push(h.join(".vst3"));
            v.push(h.join(".clap"));
        }
        for d in [
            "/usr/lib/vst3",
            "/usr/local/lib/vst3",
            "/usr/lib/clap",
            "/usr/local/lib/clap",
        ] {
            v.push(PathBuf::from(d));
        }
    }
    v
}

/// Plugin files (`.vst3` bundles/files and `.clap` files) under `dirs`, recursively.
pub fn plugin_files(dirs: &[PathBuf]) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack: Vec<(PathBuf, usize)> = dirs.iter().map(|d| (d.clone(), 0)).collect();
    while let Some((d, depth)) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else {
            continue;
        };
        for e in rd.flatten() {
            let p = e.path();
            let ext = p
                .extension()
                .and_then(|e| e.to_str())
                .map(str::to_ascii_lowercase);
            match ext.as_deref() {
                Some("vst3") => out.push(p),
                // CLAP bundles (folders) only exist on macOS.
                Some("clap") if p.is_file() || cfg!(target_os = "macos") => out.push(p),
                Some("clap") => {}
                _ if p.is_dir() && depth < 6 => stack.push((p, depth + 1)),
                _ => {}
            }
        }
    }
    out.sort();
    out.dedup();
    out
}

/// List the plugins in one file, in this process.
pub fn scan_file(path: &Path) -> Result<Vec<PluginDesc>> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase);
    match ext.as_deref() {
        Some("vst3") => vst3::scan(path),
        Some("clap") => clap::scan(path),
        _ => anyhow::bail!("not a plugin file"),
    }
}

/// Run a plugin offline over a whole piece of audio (export, "Apply to audio"), starting
/// from its saved state.
pub fn render_offline(
    desc: &PluginDesc,
    state: &[u8],
    chans: &mut [Vec<f32>],
    rate: f64,
) -> Result<()> {
    let mut p = LoadedPlugin::load(desc, rate)?;
    if !state.is_empty() {
        p.load_state(state)?;
    }
    p.idle();
    let proc = p.processor();
    let mut g = proc.lock().map_err(|_| anyhow::anyhow!("plugin lock"))?;
    g.process(chans);
    drop(g);
    Ok(())
}
