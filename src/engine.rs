//! Real-time audio I/O using cpal.
//!
//! Playback mixes the project's tracks on the fly inside the audio callback
//! (so long projects start instantly), resampling from the project rate to the
//! device rate with linear interpolation. Track gain/pan/mute/solo are read
//! live, so the mixer controls respond during playback.

use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
};

use anyhow::{Result, anyhow};
use cpal::{
    BufferSize, Device, FromSample, SampleFormat, SizedSample, Stream, StreamConfig,
    traits::{DeviceTrait, HostTrait, StreamTrait},
};
use serde::{Deserialize, Serialize};

/// Which audio devices to use (Edit ▸ Preferences ▸ Audio). `None` = system default.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct DeviceSettings {
    pub output: Option<String>,
    pub input: Option<String>,
    /// Buffer size in frames (None = driver default). Smaller = lower latency, more CPU.
    pub buffer_frames: Option<u32>,
    /// Record one channel (mixing a stereo input down) instead of up to two.
    pub record_mono: bool,
}

/// Names of the playback devices on this computer.
pub fn output_device_names() -> Vec<String> {
    cpal::default_host()
        .output_devices()
        .map(|it| it.map(|d| d.to_string()).collect())
        .unwrap_or_default()
}

/// Names of the recording devices on this computer.
pub fn input_device_names() -> Vec<String> {
    cpal::default_host()
        .input_devices()
        .map(|it| it.map(|d| d.to_string()).collect())
        .unwrap_or_default()
}

/// Find a device by name, falling back to the default. The string is a warning when the
/// chosen device wasn't found.
fn pick(input: bool, wanted: Option<&str>) -> Result<(Device, Option<String>)> {
    let host = cpal::default_host();
    if let Some(name) = wanted {
        let found = if input {
            host.input_devices()
                .ok()
                .and_then(|mut it| it.find(|d| d.to_string() == name))
        } else {
            host.output_devices()
                .ok()
                .and_then(|mut it| it.find(|d| d.to_string() == name))
        };
        if let Some(d) = found {
            return Ok((d, None));
        }
    }
    let d = if input {
        host.default_input_device()
            .ok_or_else(|| anyhow!("no input (microphone) device found"))?
    } else {
        host.default_output_device()
            .ok_or_else(|| anyhow!("no output device found"))?
    };
    let warn = wanted.map(|n| format!("“{n}” isn't available; using the default device instead."));
    Ok((d, warn))
}

use crate::{
    dsp::realtime::{Chain, LiveFx, PluginProcs},
    envelope::Envelope,
    project::Project,
    track::Channel,
};

/// State shared between the UI thread and the audio callbacks.
pub struct Shared {
    /// Playback position in project samples, stored as f64 bits.
    pos: AtomicU64,
    pub paused: AtomicBool,
    pub finished: AtomicBool,
    pub looping: AtomicBool,
    /// Peak meters (f32 bits), reset by the UI after reading.
    pub play_peak: [AtomicU32; 2],
    pub rec_peak: [AtomicU32; 2],
    /// Input is open only to show levels: captured audio is discarded.
    pub monitor_only: AtomicBool,
    /// Live (left, right) gain per track index.
    gains: Mutex<Vec<(f32, f32)>>,
    /// Live effect racks per track index, with a version number that changes on edits.
    live_fx: Mutex<(u64, Vec<Vec<LiveFx>>, PluginProcs)>,
}

impl Shared {
    fn new() -> Self {
        Self {
            pos: AtomicU64::new(0f64.to_bits()),
            paused: AtomicBool::new(false),
            finished: AtomicBool::new(false),
            looping: AtomicBool::new(false),
            play_peak: [AtomicU32::new(0), AtomicU32::new(0)],
            rec_peak: [AtomicU32::new(0), AtomicU32::new(0)],
            monitor_only: AtomicBool::new(false),
            gains: Mutex::new(Vec::new()),
            live_fx: Mutex::new((0, Vec::new(), PluginProcs::new())),
        }
    }
    pub fn pos(&self) -> f64 {
        f64::from_bits(self.pos.load(Ordering::Relaxed))
    }
    fn set_pos(&self, p: f64) {
        self.pos.store(p.to_bits(), Ordering::Relaxed);
    }
    pub fn take_peak(meter: &[AtomicU32; 2]) -> [f32; 2] {
        [
            f32::from_bits(meter[0].swap(0, Ordering::Relaxed)),
            f32::from_bits(meter[1].swap(0, Ordering::Relaxed)),
        ]
    }
}

fn bump_peak(meter: &AtomicU32, v: f32) {
    let cur = f32::from_bits(meter.load(Ordering::Relaxed));
    if v > cur {
        meter.store(v.to_bits(), Ordering::Relaxed);
    }
}

/// Immutable snapshot of the audio the callback plays.
struct Source {
    tracks: Vec<Vec<Channel>>,
    /// Volume envelope per track (None = flat).
    envelopes: Vec<Option<Envelope>>,
    start: usize,
    end: usize,
    /// project samples advanced per device frame
    step: f64,
    /// Device sample rate (live effects run at this rate).
    out_rate: f32,
}

/// Compute live gains for each track, honouring mute & solo.
fn track_gains(p: &Project) -> Vec<(f32, f32)> {
    let any_solo = p.tracks.iter().any(|t| t.solo);
    p.tracks
        .iter()
        .map(|t| {
            if t.mute || (any_solo && !t.solo) {
                (0.0, 0.0)
            } else {
                t.lr_gains()
            }
        })
        .collect()
}

#[derive(Default)]
pub struct RecordBuffer {
    /// Interleaved samples captured since the last `take_recorded`.
    pub data: Vec<f32>,
}

pub struct Engine {
    pub shared: Arc<Shared>,
    output: Option<Stream>,
    input: Option<Stream>,
    rec_buf: Arc<Mutex<RecordBuffer>>,
    pub rec_channels: usize,
    pub rec_rate: u32,
    pub devices: DeviceSettings,
    /// Set when a chosen device was missing and the default was used.
    pub device_warning: Option<String>,
    /// Live effect racks last sent to the audio callback.
    sent_fx: Vec<Vec<LiveFx>>,
    /// Running VST3/CLAP plugins by rack slot id (kept up to date by the app).
    pub plugin_procs: PluginProcs,
    sent_procs: Vec<u64>,
    /// Sample rate of the output device (plugins are run at this rate).
    out_rate: Option<u32>,
}

impl Default for Engine {
    fn default() -> Self {
        Self {
            shared: Arc::new(Shared::new()),
            output: None,
            input: None,
            rec_buf: Arc::new(Mutex::new(RecordBuffer::default())),
            rec_channels: 1,
            rec_rate: 44_100,
            sent_fx: Vec::new(),
            plugin_procs: PluginProcs::new(),
            sent_procs: Vec::new(),
            out_rate: None,
            devices: DeviceSettings::default(),
            device_warning: None,
        }
    }
}

impl Engine {
    pub fn is_playing(&self) -> bool {
        self.output.is_some()
    }
    pub fn is_paused(&self) -> bool {
        self.shared.paused.load(Ordering::Relaxed)
    }
    pub fn position(&self) -> f64 {
        self.shared.pos()
    }

    /// Push the latest gain/pan/mute/solo values (and live effect settings) to the
    /// running callback.
    pub fn update_gains(&mut self, p: &Project) {
        if let Ok(mut g) = self.shared.gains.try_lock() {
            *g = track_gains(p);
        }
        let mut procs: Vec<u64> = self.plugin_procs.keys().copied().collect();
        procs.sort_unstable();
        let changed = self.sent_fx.len() != p.tracks.len()
            || self
                .sent_fx
                .iter()
                .zip(&p.tracks)
                .any(|(a, t)| *a != t.live_fx)
            || procs != self.sent_procs;
        if changed && let Ok(mut f) = self.shared.live_fx.try_lock() {
            self.sent_fx = p.tracks.iter().map(|t| t.live_fx.clone()).collect();
            self.sent_procs = procs;
            f.0 += 1;
            f.1.clone_from(&self.sent_fx);
            f.2.clone_from(&self.plugin_procs);
        }
    }

    pub fn toggle_pause(&self) {
        self.shared.paused.fetch_xor(true, Ordering::Relaxed);
    }

    /// The output device's sample rate (what live plugins should run at).
    pub fn output_rate(&mut self) -> u32 {
        if let Some(r) = self.out_rate {
            return r;
        }
        let r = pick(false, self.devices.output.as_deref())
            .ok()
            .and_then(|(d, _)| d.default_output_config().ok())
            .map_or(48_000, |c| c.sample_rate());
        self.out_rate = Some(r);
        r
    }

    /// Forget the cached device rate (after the devices were changed).
    pub fn devices_changed(&mut self) {
        self.out_rate = None;
    }

    /// Start playback of [start, end) (end = None means to the end of the project).
    pub fn play(
        &mut self,
        p: &Project,
        start: usize,
        end: Option<usize>,
        looping: bool,
    ) -> Result<()> {
        self.stop_playback();
        let end = end.unwrap_or_else(|| p.len()).min(p.len());
        if start >= end {
            return Err(anyhow!("nothing to play"));
        }
        let (device, warn) = pick(false, self.devices.output.as_deref())?;
        if warn.is_some() {
            self.device_warning = warn;
        }
        let supported = device.default_output_config()?;
        let fmt = supported.sample_format();
        let mut config: StreamConfig = supported.into();
        if let Some(n) = self.devices.buffer_frames {
            config.buffer_size = BufferSize::Fixed(n);
        }

        let source = Arc::new(Source {
            tracks: p.tracks.iter().map(|t| t.channels.clone()).collect(),
            envelopes: p
                .tracks
                .iter()
                .map(|t| (!t.envelope.is_empty()).then(|| t.envelope.clone()))
                .collect(),
            start,
            end,
            step: p.rate as f64 / config.sample_rate as f64,
            out_rate: config.sample_rate as f32,
        });
        self.sent_fx = p.tracks.iter().map(|t| t.live_fx.clone()).collect();
        {
            let mut f = self.shared.live_fx.lock().unwrap();
            f.0 += 1;
            f.1.clone_from(&self.sent_fx);
            f.2.clone_from(&self.plugin_procs);
        }
        *self.shared.gains.lock().unwrap() = track_gains(p);
        self.shared.set_pos(start as f64);
        self.shared.finished.store(false, Ordering::Relaxed);
        self.shared.paused.store(false, Ordering::Relaxed);
        self.shared.looping.store(looping, Ordering::Relaxed);

        let build = |config: StreamConfig| match fmt {
            SampleFormat::F32 => {
                build_output::<f32>(&device, config, source.clone(), self.shared.clone())
            }
            SampleFormat::F64 => {
                build_output::<f64>(&device, config, source.clone(), self.shared.clone())
            }
            SampleFormat::I16 => {
                build_output::<i16>(&device, config, source.clone(), self.shared.clone())
            }
            SampleFormat::I32 => {
                build_output::<i32>(&device, config, source.clone(), self.shared.clone())
            }
            SampleFormat::U16 => {
                build_output::<u16>(&device, config, source.clone(), self.shared.clone())
            }
            SampleFormat::I8 => {
                build_output::<i8>(&device, config, source.clone(), self.shared.clone())
            }
            SampleFormat::U8 => {
                build_output::<u8>(&device, config, source.clone(), self.shared.clone())
            }
            other => Err(anyhow!("unsupported output sample format {other}")),
        };
        // A buffer size the driver doesn't accept falls back to its default.
        let stream = match build(config) {
            Err(_) if config.buffer_size != BufferSize::Default => build(StreamConfig {
                buffer_size: BufferSize::Default,
                ..config
            }),
            r => r,
        }?;
        stream.play()?;
        self.output = Some(stream);
        Ok(())
    }

    pub fn stop_playback(&mut self) {
        self.output = None;
        self.shared.paused.store(false, Ordering::Relaxed);
    }

    /// Called every UI frame; closes the output stream once playback has reached the end.
    pub fn poll(&mut self) {
        if self.output.is_some() && self.shared.finished.load(Ordering::Relaxed) {
            self.output = None;
        }
    }

    /// Open the input only to show its level (nothing is recorded).
    pub fn start_monitor(&mut self) -> Result<()> {
        self.start_recording()?;
        self.shared.monitor_only.store(true, Ordering::Relaxed);
        Ok(())
    }

    pub fn is_monitoring(&self) -> bool {
        self.input.is_some() && self.shared.monitor_only.load(Ordering::Relaxed)
    }

    /// Channels a recording will have (after the mono option).
    pub fn rec_track_channels(&self) -> usize {
        if self.devices.record_mono {
            1
        } else {
            self.rec_channels.min(2)
        }
    }

    /// Start capturing from the chosen (or default) input device.
    pub fn start_recording(&mut self) -> Result<()> {
        self.stop_recording();
        self.shared.monitor_only.store(false, Ordering::Relaxed);
        let (device, warn) = pick(true, self.devices.input.as_deref())?;
        if warn.is_some() {
            self.device_warning = warn;
        }
        let supported = device.default_input_config()?;
        let fmt = supported.sample_format();
        let mut config: StreamConfig = supported.into();
        if let Some(n) = self.devices.buffer_frames {
            config.buffer_size = BufferSize::Fixed(n);
        }
        self.rec_channels = config.channels.max(1) as usize;
        self.rec_rate = config.sample_rate;
        self.rec_buf.lock().unwrap().data.clear();
        let build = |config: StreamConfig| {
            let buf = self.rec_buf.clone();
            let shared = self.shared.clone();
            match fmt {
                SampleFormat::F32 => build_input::<f32>(&device, config, buf, shared),
                SampleFormat::F64 => build_input::<f64>(&device, config, buf, shared),
                SampleFormat::I16 => build_input::<i16>(&device, config, buf, shared),
                SampleFormat::I32 => build_input::<i32>(&device, config, buf, shared),
                SampleFormat::U16 => build_input::<u16>(&device, config, buf, shared),
                SampleFormat::I8 => build_input::<i8>(&device, config, buf, shared),
                SampleFormat::U8 => build_input::<u8>(&device, config, buf, shared),
                other => Err(anyhow!("unsupported input sample format {other}")),
            }
        };
        let stream = match build(config) {
            Err(_) if config.buffer_size != BufferSize::Default => build(StreamConfig {
                buffer_size: BufferSize::Default,
                ..config
            }),
            r => r,
        }?;
        stream.play()?;
        self.input = Some(stream);
        Ok(())
    }

    pub fn stop_recording(&mut self) {
        self.input = None;
        self.shared.monitor_only.store(false, Ordering::Relaxed);
    }

    /// Drain captured audio as planar channels (at `rec_rate`, `rec_channels`, max 2 kept).
    pub fn take_recorded(&self) -> Vec<Vec<f32>> {
        let data = std::mem::take(&mut self.rec_buf.lock().unwrap().data);
        let ch = self.rec_channels;
        if self.devices.record_mono {
            let k = 1.0 / ch as f32;
            return vec![
                data.chunks_exact(ch)
                    .map(|f| f.iter().sum::<f32>() * k)
                    .collect(),
            ];
        }
        let keep = ch.min(2);
        let mut out = vec![Vec::with_capacity(data.len() / ch + 1); keep];
        for frame in data.chunks_exact(ch) {
            for (c, o) in out.iter_mut().enumerate() {
                o.push(frame[c]);
            }
        }
        out
    }
}

fn build_output<T>(
    device: &cpal::Device,
    config: StreamConfig,
    src: Arc<Source>,
    shared: Arc<Shared>,
) -> Result<Stream>
where
    T: SizedSample + FromSample<f32>,
{
    let out_ch = config.channels.max(1) as usize;
    let mut gains: Vec<(f32, f32)> = Vec::new();
    // Live effects run here, on the device-rate signal of each track.
    let (mut fx_version, specs, procs) = {
        let f = shared.live_fx.lock().unwrap();
        (f.0, f.1.clone(), f.2.clone())
    };
    let mut chains: Vec<Chain> = src
        .tracks
        .iter()
        .enumerate()
        .map(|(ti, ch)| {
            Chain::with_plugins(
                specs.get(ti).map_or(&[][..], |v| v),
                src.out_rate,
                ch.len().min(2),
                &procs,
            )
        })
        .collect();
    // Reused buffers (grow only when the block size grows).
    let mut positions: Vec<f64> = Vec::new();
    let mut scratch: Vec<Vec<f32>> = vec![Vec::new(), Vec::new()];
    let mut mix: [Vec<f32>; 2] = [Vec::new(), Vec::new()];
    let stream = device.build_output_stream(
        config,
        move |data: &mut [T], _: &cpal::OutputCallbackInfo| {
            if let Ok(g) = shared.gains.try_lock() {
                gains.clone_from(&g);
            }
            if let Ok(f) = shared.live_fx.try_lock()
                && f.0 != fx_version
            {
                fx_version = f.0;
                for (ti, c) in chains.iter_mut().enumerate() {
                    c.sync_with(f.1.get(ti).map_or(&[][..], |v| v), &f.2);
                }
            }
            let frames = data.len() / out_ch;
            let paused = shared.paused.load(Ordering::Relaxed);
            if paused || shared.finished.load(Ordering::Relaxed) {
                data.iter_mut().for_each(|s| *s = T::EQUILIBRIUM);
                return;
            }
            let looping = shared.looping.load(Ordering::Relaxed);

            // Project position of every frame in this block (negative = past the end).
            positions.clear();
            let mut pos = shared.pos();
            let mut done = false;
            for _ in 0..frames {
                if !done && pos >= src.end as f64 {
                    if looping {
                        pos = src.start as f64;
                    } else {
                        done = true;
                    }
                }
                if done {
                    positions.push(-1.0);
                } else {
                    positions.push(pos);
                    pos += src.step;
                }
            }
            if done {
                shared.finished.store(true, Ordering::Relaxed);
            }
            let p0 = positions.first().copied().unwrap_or(0.0).max(0.0);

            mix_block(
                &src,
                &mut chains,
                &gains,
                &positions,
                (p0, pos),
                &mut scratch,
                &mut mix,
            );

            let (mut pk_l, mut pk_r) = (0f32, 0f32);
            for (f, frame) in data.chunks_mut(out_ch).enumerate() {
                let (l, r) = (mix[0][f], mix[1][f]);
                pk_l = pk_l.max(l.abs());
                pk_r = pk_r.max(r.abs());
                match out_ch {
                    1 => frame[0] = T::from_sample((l + r) * 0.5),
                    _ => {
                        frame[0] = T::from_sample(l);
                        frame[1] = T::from_sample(r);
                        for s in frame.iter_mut().skip(2) {
                            *s = T::EQUILIBRIUM;
                        }
                    }
                }
            }
            shared.set_pos(pos);
            bump_peak(&shared.play_peak[0], pk_l);
            bump_peak(&shared.play_peak[1], pk_r);
        },
        |e| eprintln!("output stream error: {e}"),
        None,
    )?;
    Ok(stream)
}

/// Mix one block: for every audible track, read its audio at `positions` (project
/// samples; negative = silence), apply its envelope, run its live effects, then add it to
/// `mix` with its gain and pan.
fn mix_block(
    src: &Source,
    chains: &mut [Chain],
    gains: &[(f32, f32)],
    positions: &[f64],
    (p0, p1): (f64, f64),
    scratch: &mut [Vec<f32>],
    mix: &mut [Vec<f32>; 2],
) {
    let frames = positions.len();
    for m in mix.iter_mut() {
        m.clear();
        m.resize(frames, 0.0);
    }
    for (ti, chans) in src.tracks.iter().enumerate() {
        let (gl, gr) = gains.get(ti).copied().unwrap_or((0.0, 0.0));
        if gl == 0.0 && gr == 0.0 {
            continue;
        }
        let nch = chans.len().min(2);
        for (c, buf) in scratch.iter_mut().enumerate().take(nch) {
            let ch = &chans[c];
            buf.clear();
            buf.extend(positions.iter().map(|&p| {
                if p < 0.0 {
                    return 0.0;
                }
                let i0 = p as usize;
                if i0 >= ch.len() {
                    return 0.0;
                }
                let fr = (p - i0 as f64) as f32;
                let a = ch[i0];
                let b = if i0 + 1 < ch.len() { ch[i0 + 1] } else { a };
                a + (b - a) * fr
            }));
        }
        // Envelope gains are evaluated at the block's start and end and interpolated.
        if let Some(Some(e)) = src.envelopes.get(ti) {
            let (g0, g1) = (e.gain_at(p0), e.gain_at(p1));
            for buf in scratch.iter_mut().take(nch) {
                for (f, x) in buf.iter_mut().enumerate() {
                    *x *= g0 + (g1 - g0) * (f as f32 / frames.max(1) as f32);
                }
            }
        }
        if let Some(chain) = chains.get_mut(ti)
            && chain.is_active()
        {
            chain.process(&mut scratch[..nch]);
        }
        let (left, right) = mix.split_at_mut(1);
        let r_src = if nch > 1 { &scratch[1] } else { &scratch[0] };
        for f in 0..frames {
            left[0][f] += scratch[0][f] * gl;
            right[0][f] += r_src[f] * gr;
        }
    }
}

fn build_input<T>(
    device: &cpal::Device,
    config: StreamConfig,
    buf: Arc<Mutex<RecordBuffer>>,
    shared: Arc<Shared>,
) -> Result<Stream>
where
    T: SizedSample,
    f32: FromSample<T>,
{
    let ch = config.channels.max(1) as usize;
    let stream = device.build_input_stream(
        config,
        move |data: &[T], _: &cpal::InputCallbackInfo| {
            let mut pk = [0f32; 2];
            let keep = !shared.monitor_only.load(Ordering::Relaxed);
            let mut guard = buf.lock().unwrap();
            if keep {
                guard.data.reserve(data.len());
            }
            for (i, s) in data.iter().enumerate() {
                let v: f32 = s.to_sample::<f32>();
                let c = (i % ch).min(1);
                pk[c] = pk[c].max(v.abs());
                if keep {
                    guard.data.push(v);
                }
            }
            drop(guard);
            if ch == 1 {
                pk[1] = pk[0];
            }
            bump_peak(&shared.rec_peak[0], pk[0]);
            bump_peak(&shared.rec_peak[1], pk[1]);
        },
        |e| eprintln!("input stream error: {e}"),
        None,
    )?;
    Ok(stream)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::effects::Effect;

    /// The playback mixer, run block by block at the project rate, produces the same
    /// audio as the offline mixdown used for export (gain, pan, live effects).
    #[test]
    fn playback_mix_matches_export_mixdown() {
        let mut p = Project::default();
        p.rate = 48000;
        let tone = |f: f32| {
            (0..20000)
                .map(|i| (i as f32 * f * 0.0001).sin() * 0.4)
                .collect::<Vec<f32>>()
        };
        p.add_track("a", vec![tone(1.0), tone(1.3)]);
        p.add_track("b", vec![tone(2.0)]);
        p.tracks[0].pan = -0.4;
        p.tracks[1].gain = 0.7;
        for (ti, fx) in [
            (
                0,
                Effect::Compressor {
                    threshold_db: -20.0,
                    ratio: 4.0,
                    attack_ms: 5.0,
                    release_ms: 80.0,
                    knee_db: 6.0,
                    makeup_db: 3.0,
                },
            ),
            (
                0,
                Effect::Reverb {
                    room: 0.6,
                    damping: 0.5,
                    wet_db: -6.0,
                    dry_db: 0.0,
                    width: 1.0,
                    pre_delay_ms: 10.0,
                    tail_secs: 1.0,
                },
            ),
            (
                1,
                Effect::Echo {
                    delay_secs: 0.05,
                    decay: 0.4,
                },
            ),
            (
                1,
                Effect::GraphicEq {
                    gains: [3.0, 0.0, -2.0, 0.0, 0.0, 4.0, 0.0, 0.0, -3.0, 0.0],
                },
            ),
        ] {
            p.add_live_fx(ti, fx).unwrap();
        }
        let src = Source {
            tracks: p.tracks.iter().map(|t| t.channels.clone()).collect(),
            envelopes: vec![None, None],
            start: 0,
            end: p.len(),
            step: 1.0,
            out_rate: 48000.0,
        };
        let mut chains: Vec<Chain> = p
            .tracks
            .iter()
            .map(|t| Chain::new(&t.live_fx, 48000.0, t.num_channels()))
            .collect();
        let gains = track_gains(&p);
        let (mut scratch, mut mix) = (vec![Vec::new(), Vec::new()], [Vec::new(), Vec::new()]);
        let (mut l, mut r) = (Vec::new(), Vec::new());
        let mut pos = 0usize;
        while pos < p.len() {
            let n = 333.min(p.len() - pos);
            let positions: Vec<f64> = (pos..pos + n).map(|x| x as f64).collect();
            mix_block(
                &src,
                &mut chains,
                &gains,
                &positions,
                (pos as f64, (pos + n) as f64),
                &mut scratch,
                &mut mix,
            );
            l.extend_from_slice(&mix[0]);
            r.extend_from_slice(&mix[1]);
            pos += n;
        }
        let m = p.mixdown(0, p.len(), false);
        let diff = |a: &[f32], b: &[f32]| {
            a.iter()
                .zip(b)
                .map(|(x, y)| (x - y).abs())
                .fold(0f32, f32::max)
        };
        assert!(
            diff(&l, &m[0]) < 1e-4,
            "left differs by {}",
            diff(&l, &m[0])
        );
        assert!(
            diff(&r, &m[1]) < 1e-4,
            "right differs by {}",
            diff(&r, &m[1])
        );
        assert!(l.iter().any(|x| x.abs() > 0.1));
    }
}
