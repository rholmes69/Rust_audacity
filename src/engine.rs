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

use crate::{envelope::Envelope, project::Project, track::Channel};

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

    /// Push the latest gain/pan/mute/solo values to the running callback.
    pub fn update_gains(&self, p: &Project) {
        if let Ok(mut g) = self.shared.gains.try_lock() {
            *g = track_gains(p);
        }
    }

    pub fn toggle_pause(&self) {
        self.shared.paused.fetch_xor(true, Ordering::Relaxed);
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
        });
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
    let stream = device.build_output_stream(
        config,
        move |data: &mut [T], _: &cpal::OutputCallbackInfo| {
            if let Ok(g) = shared.gains.try_lock() {
                gains.clone_from(&g);
            }
            let paused = shared.paused.load(Ordering::Relaxed);
            let looping = shared.looping.load(Ordering::Relaxed);
            let mut pos = shared.pos();
            let (mut pk_l, mut pk_r) = (0f32, 0f32);
            // Envelope gains are evaluated at the block's start and end and interpolated.
            let frames = data.len() / out_ch;
            let block_end = pos + frames as f64 * src.step;
            let env: Vec<Option<(f32, f32)>> = src
                .envelopes
                .iter()
                .map(|e| e.as_ref().map(|e| (e.gain_at(pos), e.gain_at(block_end))))
                .collect();
            for (frame_i, frame) in data.chunks_mut(out_ch).enumerate() {
                let t = frame_i as f32 / frames.max(1) as f32;
                let (mut l, mut r) = (0f32, 0f32);
                if !paused && !shared.finished.load(Ordering::Relaxed) {
                    if pos >= src.end as f64 {
                        if looping {
                            pos = src.start as f64;
                        } else {
                            shared.finished.store(true, Ordering::Relaxed);
                        }
                    }
                    if pos < src.end as f64 {
                        let i0 = pos as usize;
                        let fr = (pos - i0 as f64) as f32;
                        for (ti, chans) in src.tracks.iter().enumerate() {
                            let (mut gl, mut gr) = gains.get(ti).copied().unwrap_or((0.0, 0.0));
                            if gl == 0.0 && gr == 0.0 {
                                continue;
                            }
                            if let Some(Some((g0, g1))) = env.get(ti) {
                                let g = g0 + (g1 - g0) * t;
                                gl *= g;
                                gr *= g;
                            }
                            let sample = |c: &Channel| -> f32 {
                                if i0 >= c.len() {
                                    return 0.0;
                                }
                                let a = c[i0];
                                let b = if i0 + 1 < c.len() { c[i0 + 1] } else { a };
                                a + (b - a) * fr
                            };
                            let sl = sample(&chans[0]);
                            let sr = chans.get(1).map_or(sl, sample);
                            l += sl * gl;
                            r += sr * gr;
                        }
                        pos += src.step;
                    }
                }
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
