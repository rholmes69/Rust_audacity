//! CLAP hosting (via the `clack-host` crate): loading `.clap` files, processing, parameters,
//! state, timers and (on Windows) the plugin's window.

use std::{
    any::Any,
    cell::RefCell,
    ffi::{CStr, CString},
    path::Path,
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use anyhow::{Result, anyhow, bail};
use clack_extensions::{
    audio_ports::{AudioPortInfoBuffer, PluginAudioPorts},
    gui::{GuiSize, HostGui, HostGuiImpl, PluginGui},
    latency::{HostLatency, HostLatencyImpl},
    log::{HostLog, HostLogImpl, LogSeverity},
    params::{
        HostParams, HostParamsImplMainThread, HostParamsImplShared, ParamClearFlags,
        ParamInfoBuffer, ParamInfoFlags, ParamRescanFlags, PluginParams,
    },
    state::PluginState,
    timer::{HostTimer, HostTimerImpl, PluginTimer, TimerId},
};
use clack_host::{events::event_types::ParamValueEvent, prelude::*};

#[cfg(any(windows, wincheck))]
use clack_extensions::gui::{GuiApiType, GuiConfiguration};

use super::{
    AudioProcess, ParamInfo, ParamQueue, PluginDesc, PluginFormat, SharedProcess, vst3::write_back,
};

fn host_info() -> Result<HostInfo> {
    HostInfo::new(
        "Rust Audacity",
        "Rust Audacity",
        "https://github.com/rholmes69/Rust_audacity",
        env!("CARGO_PKG_VERSION"),
    )
    .map_err(|e| anyhow!("{e:?}"))
}

fn load_entry(path: &Path) -> Result<PluginEntry> {
    if cfg!(target_os = "macos") && path.is_dir() {
        // Bundles are supported by clack on macOS.
    }
    unsafe { PluginEntry::load(path) }.map_err(|e| anyhow!("loading {}: {e:?}", path.display()))
}

/// List the audio effects in a `.clap` file.
pub fn scan(path: &Path) -> Result<Vec<PluginDesc>> {
    let entry = load_entry(path)?;
    let factory = entry
        .get_plugin_factory()
        .ok_or_else(|| anyhow!("no plugin factory"))?;
    let mut out = Vec::new();
    for d in factory.plugin_descriptors() {
        let Some(id) = d.id() else { continue };
        let features: Vec<String> = d
            .features()
            .map(|f| f.to_string_lossy().into_owned())
            .collect();
        out.push(PluginDesc {
            format: PluginFormat::Clap,
            path: path.to_path_buf(),
            id: id.to_string_lossy().into_owned(),
            name: d.name().map_or_else(
                || id.to_string_lossy().into_owned(),
                |n| n.to_string_lossy().into_owned(),
            ),
            vendor: d
                .vendor()
                .map(|v| v.to_string_lossy().into_owned())
                .unwrap_or_default(),
            is_instrument: features.iter().any(|f| f == "instrument"),
            category: features.join(" "),
        });
    }
    Ok(out)
}

// ------------------------------------------------------------------ host handlers

pub struct RaHost;

#[derive(Default)]
pub struct Shared {
    callback_requested: AtomicBool,
    gui_events: Mutex<Vec<GuiEvent>>,
    params: OnceLock<Option<PluginParams>>,
    state: OnceLock<Option<PluginState>>,
    gui: OnceLock<Option<PluginGui>>,
    timer: OnceLock<Option<PluginTimer>>,
    ports: OnceLock<Option<PluginAudioPorts>>,
}

impl<'a> SharedHandler<'a> for Shared {
    fn initializing(&self, instance: InitializingPluginHandle<'a>) {
        let _ = self.params.set(instance.get_extension());
        let _ = self.state.set(instance.get_extension());
        let _ = self.gui.set(instance.get_extension());
        let _ = self.timer.set(instance.get_extension());
        let _ = self.ports.set(instance.get_extension());
    }
    fn request_restart(&self) {}
    fn request_process(&self) {}
    fn request_callback(&self) {
        self.callback_requested.store(true, Ordering::Release);
    }
}

impl HostLogImpl for Shared {
    fn log(&self, severity: LogSeverity, message: &str) {
        if matches!(
            severity,
            LogSeverity::Error | LogSeverity::Fatal | LogSeverity::HostMisbehaving
        ) {
            eprintln!("[plugin] {message}");
        }
    }
}

impl HostParamsImplShared for Shared {
    fn request_flush(&self) {}
}

/// What the plugin's window asked for (handled in `idle`).
#[derive(Debug)]
#[cfg_attr(not(any(windows, wincheck)), allow(dead_code))]
enum GuiEvent {
    Resize(u32, u32),
    Closed,
}

pub struct MainThread {
    timers: RefCell<Vec<(u32, Duration, Instant)>>,
    next_timer: RefCell<u32>,
}

impl<'a> MainThreadHandler<'a> for MainThread {}

impl HostParamsImplMainThread for MainThread {
    fn rescan(&self, _flags: ParamRescanFlags) {}
    fn clear(&self, _id: ClapId, _flags: ParamClearFlags) {}
}

impl HostLatencyImpl for MainThread {
    fn changed(&self) {}
}

impl HostGuiImpl for Shared {
    fn resize_hints_changed(&self) {}
    fn request_resize(&self, size: GuiSize) -> Result<(), HostError> {
        if let Ok(mut e) = self.gui_events.lock() {
            e.push(GuiEvent::Resize(size.width, size.height));
        }
        Ok(())
    }
    fn request_show(&self) -> Result<(), HostError> {
        Ok(())
    }
    fn request_hide(&self) -> Result<(), HostError> {
        Ok(())
    }
    fn closed(&self, _was_destroyed: bool) {
        if let Ok(mut e) = self.gui_events.lock() {
            e.push(GuiEvent::Closed);
        }
    }
}

impl HostTimerImpl for MainThread {
    fn register_timer(&self, period_ms: u32) -> Result<TimerId, HostError> {
        let mut n = self.next_timer.borrow_mut();
        *n += 1;
        let period = Duration::from_millis(period_ms.max(10) as u64);
        self.timers
            .borrow_mut()
            .push((*n, period, Instant::now() + period));
        Ok(TimerId(*n))
    }
    fn unregister_timer(&self, id: TimerId) -> Result<(), HostError> {
        self.timers.borrow_mut().retain(|t| t.0 != id.0);
        Ok(())
    }
}

impl HostHandlers for RaHost {
    type Shared<'a> = Shared;
    type MainThread<'a> = MainThread;
    type AudioProcessor<'a> = ();

    fn declare_extensions(builder: &mut HostExtensions<Self>, _shared: &Self::Shared<'_>) {
        builder
            .register::<HostLog>()
            .register::<HostParams>()
            .register::<HostLatency>()
            .register::<HostGui>()
            .register::<HostTimer>();
    }
}

// ------------------------------------------------------------------ audio side

struct Processor {
    proc: Option<StartedPluginAudioProcessor<RaHost>>,
    params: Option<PluginParams>,
    queue: ParamQueue,
    in_ch: usize,
    max_block: usize,
    ins: Vec<Vec<f32>>,
    outs: Vec<Vec<f32>>,
    in_ports: AudioPorts,
    out_ports: AudioPorts,
    events: EventBuffer,
    out_events: EventBuffer,
    pending: Vec<(u32, f64)>,
    steady: u64,
}

// The processor is used by one thread at a time (behind the mutex).
unsafe impl Send for Processor {}

impl Processor {
    fn queued_events(&mut self) {
        self.pending.clear();
        self.queue.drain_into(&mut self.pending);
        self.events.clear();
        for &(id, v) in &self.pending {
            if let Some(cid) = ClapId::from_raw(id) {
                self.events
                    .push(&ParamValueEvent::new(0, cid, Pckn::match_all(), v));
            }
        }
        self.out_events.clear();
    }

    fn run(&mut self, n: usize) {
        self.queued_events();
        let Some(proc) = self.proc.as_mut() else {
            return;
        };
        let input_events = InputEvents::from_buffer(&self.events);
        let mut output_events = OutputEvents::from_buffer(&mut self.out_events);
        let input = self.in_ports.with_input_buffers((self.in_ch > 0).then(|| {
            AudioPortBuffer {
                latency: 0,
                channels: AudioPortBufferType::f32_input_only(
                    self.ins
                        .iter_mut()
                        .map(|b| InputChannel::variable(&mut b[..n])),
                ),
            }
        }));
        let mut output = self.out_ports.with_output_buffers([AudioPortBuffer {
            latency: 0,
            channels: AudioPortBufferType::f32_output_only(
                self.outs.iter_mut().map(|b| &mut b[..n]),
            ),
        }]);
        let _ = proc.process(
            &input,
            &mut output,
            &input_events,
            &mut output_events,
            Some(self.steady),
            None,
        );
        self.steady += n as u64;
    }
}

impl AudioProcess for Processor {
    fn process(&mut self, chans: &mut [Vec<f32>]) {
        let len = chans.first().map_or(0, Vec::len);
        let mut off = 0;
        while off < len {
            let n = (len - off).min(self.max_block);
            for (c, buf) in self.ins.iter_mut().enumerate() {
                let src = &chans[c.min(chans.len() - 1)];
                buf[..n].copy_from_slice(&src[off..off + n]);
            }
            for b in self.outs.iter_mut() {
                b[..n].fill(0.0);
            }
            self.run(n);
            write_back(chans, &self.outs, off, n);
            off += n;
        }
    }

    fn flush_params(&mut self) {
        if self.queue.is_empty() {
            return;
        }
        self.queued_events();
        let (Some(proc), Some(params)) = (self.proc.as_mut(), self.params) else {
            return;
        };
        let input_events = InputEvents::from_buffer(&self.events);
        let mut output_events = OutputEvents::from_buffer(&mut self.out_events);
        params.flush_active(&mut proc.plugin_handle(), &input_events, &mut output_events);
    }

    fn as_any_mut(&mut self) -> Option<&mut dyn Any> {
        Some(self)
    }
}

// ------------------------------------------------------------------ main-thread side

pub struct ClapPlugin {
    desc: PluginDesc,
    shared: SharedProcess,
    queue: ParamQueue,
    rate: f64,
    max_block: usize,
    in_ch: usize,
    out_ch: usize,
    /// Values we set that the plugin may not have processed yet.
    pending_values: std::collections::HashMap<u32, f64>,
    #[cfg(any(windows, wincheck))]
    editor: Option<ClapEditor>,
    // Dropped last (fields drop in order): the instance before the library.
    instance: PluginInstance<RaHost>,
    _entry: PluginEntry,
}

#[cfg(any(windows, wincheck))]
enum ClapEditor {
    Floating,
    Embedded(super::win_editor::Editor),
}

fn ext<T: Copy>(cell: &OnceLock<Option<T>>) -> Option<T> {
    cell.get().copied().flatten()
}

impl ClapPlugin {
    pub fn load(desc: &PluginDesc, rate: f64, max_block: usize) -> Result<Self> {
        let entry = load_entry(&desc.path)?;
        let id = CString::new(desc.id.clone())?;
        let mut instance = PluginInstance::<RaHost>::new(
            |_| Shared::default(),
            |_| MainThread {
                timers: RefCell::new(Vec::new()),
                next_timer: RefCell::new(0),
            },
            &entry,
            &id,
            &host_info()?,
        )
        .map_err(|e| anyhow!("{} could not be created: {e:?}", desc.name))?;

        // Channel counts of the main ports.
        let (in_ch, out_ch) = {
            let ports = instance.access_shared_handler(|s| ext(&s.ports));
            let h = instance.plugin_handle();
            let ch = |is_input: bool| -> usize {
                let Some(p) = ports else { return 2 };
                if p.count(&h, is_input) == 0 {
                    return 0;
                }
                let mut buf = AudioPortInfoBuffer::new();
                p.get(&h, 0, is_input, &mut buf)
                    .map_or(2, |i| i.channel_count as usize)
            };
            (ch(true), ch(false))
        };
        if out_ch == 0 {
            bail!("{} has no audio output", desc.name);
        }
        let queue = ParamQueue::default();
        let params = instance.access_shared_handler(|s| ext(&s.params));
        let proc = Processor {
            proc: None,
            params,
            queue: queue.clone(),
            in_ch,
            max_block,
            ins: vec![vec![0.0; max_block]; in_ch],
            outs: vec![vec![0.0; max_block]; out_ch],
            in_ports: AudioPorts::with_capacity(in_ch.max(1), 1),
            out_ports: AudioPorts::with_capacity(out_ch, 1),
            events: EventBuffer::with_capacity(64),
            out_events: EventBuffer::with_capacity(64),
            pending: Vec::with_capacity(64),
            steady: 0,
        };
        let mut p = Self {
            desc: desc.clone(),
            shared: Arc::new(Mutex::new(Box::new(proc))),
            queue,
            rate,
            max_block,
            in_ch,
            out_ch,
            pending_values: Default::default(),
            #[cfg(any(windows, wincheck))]
            editor: None,
            instance,
            _entry: entry,
        };
        p.activate()?;
        Ok(p)
    }

    fn activate(&mut self) -> Result<()> {
        let config = PluginAudioConfiguration {
            sample_rate: self.rate,
            min_frames_count: 1,
            max_frames_count: self.max_block as u32,
        };
        let stopped = self
            .instance
            .activate(|_, _| (), config)
            .map_err(|e| anyhow!("{} could not start: {e:?}", self.desc.name))?;
        let started = stopped
            .start_processing()
            .map_err(|e| anyhow!("{} could not start processing: {e:?}", self.desc.name))?;
        let mut g = self.shared.lock().map_err(|_| anyhow!("plugin lock"))?;
        if let Some(p) = g.as_any_mut().and_then(|a| a.downcast_mut::<Processor>()) {
            p.proc = Some(started);
        }
        Ok(())
    }

    fn deactivate(&mut self) {
        let started = {
            let Ok(mut g) = self.shared.lock() else {
                return;
            };
            g.as_any_mut()
                .and_then(|a| a.downcast_mut::<Processor>())
                .and_then(|p| p.proc.take())
        };
        if let Some(s) = started {
            let stopped = s.stop_processing();
            self.instance.deactivate(stopped);
        }
    }

    pub fn desc(&self) -> &PluginDesc {
        &self.desc
    }

    pub fn processor(&self) -> SharedProcess {
        self.shared.clone()
    }

    pub fn set_rate(&mut self, rate: f64) -> Result<()> {
        if (rate - self.rate).abs() < 0.5 {
            return Ok(());
        }
        self.deactivate();
        self.rate = rate;
        self.activate()
    }

    pub fn params(&mut self) -> Vec<ParamInfo> {
        let Some(params) = self.instance.access_shared_handler(|s| ext(&s.params)) else {
            return Vec::new();
        };
        let h = self.instance.plugin_handle();
        let mut out = Vec::new();
        let mut buf = ParamInfoBuffer::new();
        for i in 0..params.count(&h) {
            let Some(info) = params.get_info(&h, i, &mut buf) else {
                continue;
            };
            if info
                .flags
                .intersects(ParamInfoFlags::IS_HIDDEN | ParamInfoFlags::IS_READONLY)
            {
                continue;
            }
            let id = info.id.get();
            let stepped = info.flags.contains(ParamInfoFlags::IS_STEPPED);
            let value = self
                .pending_values
                .get(&id)
                .copied()
                .or_else(|| params.get_value(&h, info.id))
                .unwrap_or(info.default_value);
            out.push(ParamInfo {
                id,
                name: String::from_utf8_lossy(info.name).into_owned(),
                unit: String::new(),
                min: info.min_value,
                max: info.max_value,
                default: info.default_value,
                value,
                steps: if stepped {
                    (info.max_value - info.min_value).round().max(1.0) as u32
                } else {
                    0
                },
                is_bypass: info.flags.contains(ParamInfoFlags::IS_BYPASS),
            });
        }
        out
    }

    pub fn param_text(&mut self, id: u32, value: f64) -> Option<String> {
        let params = self.instance.access_shared_handler(|s| ext(&s.params))?;
        let h = self.instance.plugin_handle();
        let mut buf = [0u8; 128];
        let cid = ClapId::from_raw(id)?;
        params
            .value_to_text(&h, cid, value, &mut buf)
            .ok()
            .map(|b| String::from_utf8_lossy(b).into_owned())
    }

    pub fn set_param(&mut self, id: u32, value: f64) {
        self.pending_values.insert(id, value);
        self.queue.push(id, value);
    }

    pub fn idle(&mut self) {
        // Deliver parameter changes while nothing is playing.
        if !self.queue.is_empty()
            && let Ok(mut g) = self.shared.try_lock()
        {
            g.flush_params();
        }
        if self.queue.is_empty() {
            self.pending_values.clear();
        }
        if self
            .instance
            .access_shared_handler(|s| s.callback_requested.swap(false, Ordering::AcqRel))
        {
            self.instance.call_on_main_thread_callback();
        }
        // Timers.
        let now = Instant::now();
        let due: Vec<u32> = self.instance.access_handler(|m| {
            let mut t = m.timers.borrow_mut();
            let mut due = Vec::new();
            for (id, period, next) in t.iter_mut() {
                if *next <= now {
                    due.push(*id);
                    *next = now + *period;
                }
            }
            due
        });
        if !due.is_empty()
            && let Some(timer) = self.instance.access_shared_handler(|s| ext(&s.timer))
        {
            let h = self.instance.plugin_handle();
            for id in due {
                timer.on_timer(&h, TimerId(id));
            }
        }
        let events: Vec<GuiEvent> = self.instance.access_shared_handler(|s| {
            s.gui_events
                .lock()
                .map(|mut e| e.drain(..).collect())
                .unwrap_or_default()
        });
        #[cfg(any(windows, wincheck))]
        {
            let mut close = false;
            for e in events {
                match e {
                    GuiEvent::Closed => close = true,
                    GuiEvent::Resize(w, h) => {
                        if let Some(ClapEditor::Embedded(ed)) = &self.editor {
                            ed.resize_client(w as i32, h as i32);
                            if let Some(gui) = self.instance.access_shared_handler(|s| ext(&s.gui))
                            {
                                let _ = gui.set_size(
                                    &self.instance.plugin_handle(),
                                    GuiSize {
                                        width: w,
                                        height: h,
                                    },
                                );
                            }
                        }
                    }
                }
            }
            if let Some(ClapEditor::Embedded(ed)) = &self.editor {
                if ed.closed() {
                    close = true;
                } else if let Some((w, h)) = ed.take_user_resize()
                    && let Some(gui) = self.instance.access_shared_handler(|s| ext(&s.gui))
                {
                    let h_ = self.instance.plugin_handle();
                    let size = gui
                        .adjust_size(
                            &h_,
                            GuiSize {
                                width: w as u32,
                                height: h as u32,
                            },
                        )
                        .unwrap_or(GuiSize {
                            width: w as u32,
                            height: h as u32,
                        });
                    let _ = gui.set_size(&h_, size);
                    if size.width as i32 != w || size.height as i32 != h {
                        ed.resize_client(size.width as i32, size.height as i32);
                    }
                }
            }
            if close {
                self.close_editor();
            }
        }
        #[cfg(not(any(windows, wincheck)))]
        drop(events);
    }

    pub fn save_state(&mut self) -> Result<Vec<u8>> {
        self.idle();
        let state = self
            .instance
            .access_shared_handler(|s| ext(&s.state))
            .ok_or_else(|| anyhow!("{} can't save its settings", self.desc.name))?;
        let mut out = Vec::new();
        let _g = self.shared.lock();
        state
            .save(&self.instance.plugin_handle(), &mut out)
            .map_err(|e| anyhow!("{}: {e:?}", self.desc.name))?;
        Ok(out)
    }

    pub fn load_state(&mut self, data: &[u8]) -> Result<()> {
        let state = self
            .instance
            .access_shared_handler(|s| ext(&s.state))
            .ok_or_else(|| anyhow!("{} can't load settings", self.desc.name))?;
        let _g = self.shared.lock();
        let mut r = std::io::Cursor::new(data);
        state
            .load(&self.instance.plugin_handle(), &mut r)
            .map_err(|e| anyhow!("{}: {e:?}", self.desc.name))
    }

    pub fn has_editor(&self) -> bool {
        cfg!(any(windows, wincheck))
            && self
                .instance
                .access_shared_handler(|s| ext(&s.gui))
                .is_some()
    }

    #[cfg(any(windows, wincheck))]
    pub fn open_editor(&mut self, title: &str) -> Result<()> {
        if self.editor.is_some() {
            return Ok(());
        }
        let gui = self
            .instance
            .access_shared_handler(|s| ext(&s.gui))
            .ok_or_else(|| anyhow!("{} has no window", self.desc.name))?;
        let h = self.instance.plugin_handle();
        let title_c = CString::new(title.replace('\0', "")).unwrap_or_default();
        if gui.is_api_supported(
            &h,
            GuiConfiguration {
                api_type: GuiApiType::WIN32,
                is_floating: true,
            },
        ) {
            gui.create(
                &h,
                GuiConfiguration {
                    api_type: GuiApiType::WIN32,
                    is_floating: true,
                },
            )
            .map_err(|e| anyhow!("{e:?}"))?;
            gui.suggest_title(&h, &title_c);
            gui.show(&h).map_err(|e| anyhow!("{e:?}"))?;
            self.editor = Some(ClapEditor::Floating);
            return Ok(());
        }
        gui.create(
            &h,
            GuiConfiguration {
                api_type: GuiApiType::WIN32,
                is_floating: false,
            },
        )
        .map_err(|e| anyhow!("{} couldn't open its window: {e:?}", self.desc.name))?;
        let size = gui.get_size(&h).unwrap_or(GuiSize {
            width: 640,
            height: 400,
        });
        let ed = super::win_editor::Editor::open_window(
            title,
            size.width as i32,
            size.height as i32,
            gui.can_resize(&h),
        )?;
        unsafe {
            gui.set_parent(
                &h,
                clack_extensions::gui::Window::from_win32_hwnd(ed.hwnd()),
            )
            .map_err(|e| anyhow!("{e:?}"))?;
        }
        let _ = gui.show(&h);
        self.editor = Some(ClapEditor::Embedded(ed));
        Ok(())
    }

    #[cfg(not(any(windows, wincheck)))]
    pub fn open_editor(&mut self, _title: &str) -> Result<()> {
        bail!("plugin windows are only available on Windows for now; use the sliders")
    }

    pub fn close_editor(&mut self) {
        #[cfg(any(windows, wincheck))]
        if let Some(ed) = self.editor.take() {
            if let Some(gui) = self.instance.access_shared_handler(|s| ext(&s.gui)) {
                let h = self.instance.plugin_handle();
                let _ = gui.hide(&h);
                gui.destroy(&h);
            }
            if let ClapEditor::Embedded(w) = ed {
                w.close();
            }
        }
    }

    pub fn editor_open(&self) -> bool {
        #[cfg(any(windows, wincheck))]
        {
            self.editor.is_some()
        }
        #[cfg(not(any(windows, wincheck)))]
        {
            false
        }
    }

    #[allow(dead_code)]
    fn channels(&self) -> (usize, usize) {
        (self.in_ch, self.out_ch)
    }
}

impl Drop for ClapPlugin {
    fn drop(&mut self) {
        self.close_editor();
        self.deactivate();
        if let Ok(mut g) = self.shared.lock() {
            *g = Box::new(super::Bypass);
        }
    }
}

#[allow(dead_code)]
fn cstr_lossy(c: &CStr) -> String {
    c.to_string_lossy().into_owned()
}
