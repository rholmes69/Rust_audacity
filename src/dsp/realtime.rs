//! Real-time ("live") effects: a per-track rack of effects that run while the audio plays,
//! without changing the recorded audio. Settings can change during playback without
//! clicks (filters and envelopes keep their state). Export and Mix and Render apply the
//! same processing offline, so what you hear is what you get.

use serde::{Deserialize, Serialize};

use super::{
    biquad::{BUTTERWORTH_Q, Biquad},
    dynamics::{Compressor, DeEsser, Gate, LiveLimiter},
    space::{LiveReverb, LiveVocalRemover, stereo_width},
    tone::{DistortionKind, EQ_BANDS, distort},
};
use std::collections::HashMap;

use crate::{
    effects::Effect,
    plugins::{PluginDesc, SharedProcess},
};

/// One slot in a track's live effect rack: a built-in effect, or a third-party plugin.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LiveFx {
    pub id: u64,
    /// The built-in effect (ignored when `plugin` is set).
    pub effect: Effect,
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugin: Option<PluginSlot>,
}

/// A VST3/CLAP plugin in a rack, with its saved settings.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PluginSlot {
    pub desc: PluginDesc,
    /// The plugin's own state blob (captured from the running plugin before saving).
    #[serde(with = "crate::metadata::b64", default)]
    pub state: Vec<u8>,
}

impl LiveFx {
    pub fn builtin(id: u64, effect: Effect) -> Self {
        Self {
            id,
            effect,
            enabled: true,
            plugin: None,
        }
    }

    pub fn plugin(id: u64, desc: PluginDesc) -> Self {
        Self {
            id,
            effect: Effect::Amplify { db: 0.0 },
            enabled: true,
            plugin: Some(PluginSlot {
                desc,
                state: Vec::new(),
            }),
        }
    }

    pub fn name(&self) -> &str {
        match &self.plugin {
            Some(p) => &p.desc.name,
            None => self.effect.name(),
        }
    }
}

/// Running plugin processors, by rack slot id (owned by the app, shared with playback).
pub type PluginProcs = HashMap<u64, SharedProcess>;

/// Effects that can run live. The others need the whole recording (normalize, fades,
/// noise reduction, tempo…) and are only available as regular, destructive effects.
pub fn supports(fx: &Effect) -> bool {
    matches!(
        fx,
        Effect::Amplify { .. }
            | Effect::Invert
            | Effect::Echo { .. }
            | Effect::LowPass { .. }
            | Effect::HighPass { .. }
            | Effect::HumRemoval { .. }
            | Effect::DeEsser { .. }
            | Effect::Compressor { .. }
            | Effect::Limiter { .. }
            | Effect::NoiseGate { .. }
            | Effect::GraphicEq { .. }
            | Effect::BassTreble { .. }
            | Effect::Distortion { .. }
            | Effect::Reverb { .. }
            | Effect::StereoWidener { .. }
            | Effect::VocalRemover { .. }
    )
}

/// The live-capable effects, with default settings, in menu order.
pub fn catalog() -> Vec<Effect> {
    Effect::catalog().into_iter().filter(supports).collect()
}

enum State {
    Gain(f32),
    /// A cascade of filters per channel.
    Filters(Vec<Vec<Biquad>>),
    Echo {
        ring: Vec<Vec<f32>>,
        idx: usize,
        decay: f32,
    },
    Comp(Compressor),
    Limit(LiveLimiter),
    Gate(Gate),
    DeEss(DeEsser),
    Distort {
        kind: DistortionKind,
        drive_db: f32,
        mix: f32,
        output_db: f32,
    },
    Reverb(Box<LiveReverb>),
    Widen {
        width: f32,
        mid_db: f32,
        side_db: f32,
    },
    Vocal(LiveVocalRemover),
}

/// Filter cascade for the filter-type effects (the same length for any settings of one
/// effect, so settings can change without resetting the filters).
fn filter_cascade(fx: &Effect, rate: f32) -> Option<Vec<Biquad>> {
    let nyq_ok = |f: f32| f < rate * 0.45;
    Some(match *fx {
        Effect::LowPass { cutoff_hz } => vec![Biquad::lowpass(cutoff_hz, BUTTERWORTH_Q, rate)],
        Effect::HighPass { cutoff_hz } => vec![Biquad::highpass(cutoff_hz, BUTTERWORTH_Q, rate)],
        Effect::HumRemoval {
            base_hz,
            harmonics,
            q,
        } => (1..=harmonics.max(1))
            .map(|h| base_hz * h as f32)
            .filter(|&f| nyq_ok(f))
            .map(|f| Biquad::notch(f, q, rate))
            .collect(),
        Effect::GraphicEq { gains } => EQ_BANDS
            .iter()
            .zip(gains)
            .filter(|(f, _)| nyq_ok(**f))
            .map(|(&f, g)| Biquad::peaking(f, 1.41, g, rate))
            .collect(),
        Effect::BassTreble { bass_db, treble_db } => vec![
            Biquad::low_shelf(250.0, bass_db, rate),
            Biquad::high_shelf(4000.0, treble_db, rate),
        ],
        _ => return None,
    })
}

fn build(fx: &Effect, rate: f32, channels: usize) -> Option<State> {
    if let Some(c) = filter_cascade(fx, rate) {
        return Some(State::Filters(vec![c; channels]));
    }
    Some(match *fx {
        Effect::Amplify { db } => State::Gain(10f32.powf(db / 20.0)),
        Effect::Invert => State::Gain(-1.0),
        Effect::Echo { delay_secs, decay } => State::Echo {
            ring: vec![vec![0.0; ((delay_secs * rate).round() as usize).max(1)]; channels],
            idx: 0,
            decay,
        },
        Effect::Compressor {
            threshold_db,
            ratio,
            attack_ms,
            release_ms,
            knee_db,
            makeup_db,
        } => State::Comp(Compressor::new(
            rate,
            threshold_db,
            ratio,
            attack_ms,
            release_ms,
            knee_db,
            makeup_db,
        )),
        Effect::Limiter {
            ceiling_db,
            release_ms,
        } => State::Limit(LiveLimiter::new(rate, ceiling_db, release_ms)),
        Effect::NoiseGate {
            threshold_db,
            reduction_db,
            attack_ms,
            hold_ms,
            release_ms,
        } => State::Gate(Gate::new(
            rate,
            threshold_db,
            reduction_db,
            attack_ms,
            hold_ms,
            release_ms,
        )),
        Effect::DeEsser {
            threshold_db,
            freq_hz,
            max_reduction_db,
        } => State::DeEss(DeEsser::new(
            rate,
            channels,
            threshold_db,
            freq_hz,
            max_reduction_db,
        )),
        Effect::Distortion {
            kind,
            drive_db,
            mix,
            output_db,
        } => State::Distort {
            kind,
            drive_db,
            mix,
            output_db,
        },
        Effect::Reverb {
            room,
            damping,
            wet_db,
            dry_db,
            width,
            pre_delay_ms,
            ..
        } => State::Reverb(Box::new(LiveReverb::new(
            rate,
            room,
            damping,
            wet_db,
            dry_db,
            width,
            pre_delay_ms,
        ))),
        Effect::StereoWidener {
            width,
            mid_db,
            side_db,
        } => State::Widen {
            width,
            mid_db,
            side_db,
        },
        Effect::VocalRemover { keep_bass_hz } => {
            State::Vocal(LiveVocalRemover::new(rate, keep_bass_hz))
        }
        _ => return None,
    })
}

/// One running effect.
pub struct Processor {
    pub id: u64,
    effect: Effect,
    rate: f32,
    channels: usize,
    state: State,
}

impl Processor {
    pub fn new(id: u64, fx: &Effect, rate: f32, channels: usize) -> Option<Self> {
        Some(Self {
            id,
            effect: *fx,
            rate,
            channels,
            state: build(fx, rate, channels)?,
        })
    }

    /// Apply new settings, keeping filter/envelope/reverb state where possible.
    pub fn update(&mut self, fx: &Effect) {
        if *fx == self.effect {
            return;
        }
        let rate = self.rate;
        let same_kind = std::mem::discriminant(fx) == std::mem::discriminant(&self.effect);
        let kept = same_kind
            && match (&mut self.state, fx) {
                (State::Filters(per_ch), _) => match filter_cascade(fx, rate) {
                    Some(new) if per_ch.first().is_some_and(|c| c.len() == new.len()) => {
                        for ch in per_ch.iter_mut() {
                            for (old, n) in ch.iter_mut().zip(&new) {
                                old.set_coefficients(n);
                            }
                        }
                        true
                    }
                    _ => false,
                },
                (State::Gain(g), Effect::Amplify { db }) => {
                    *g = 10f32.powf(db / 20.0);
                    true
                }
                (
                    State::Echo { ring, idx, decay },
                    Effect::Echo {
                        delay_secs,
                        decay: d,
                    },
                ) => {
                    let n = ((delay_secs * rate).round() as usize).max(1);
                    if ring.first().is_some_and(|r| r.len() != n) {
                        ring.iter_mut().for_each(|r| *r = vec![0.0; n]);
                        *idx = 0;
                    }
                    *decay = *d;
                    true
                }
                (
                    State::Comp(c),
                    Effect::Compressor {
                        threshold_db,
                        ratio,
                        attack_ms,
                        release_ms,
                        knee_db,
                        makeup_db,
                    },
                ) => {
                    c.set(
                        rate,
                        *threshold_db,
                        *ratio,
                        *attack_ms,
                        *release_ms,
                        *knee_db,
                        *makeup_db,
                    );
                    true
                }
                (
                    State::Limit(l),
                    Effect::Limiter {
                        ceiling_db,
                        release_ms,
                    },
                ) => {
                    l.set(rate, *ceiling_db, *release_ms);
                    true
                }
                (
                    State::Gate(g),
                    Effect::NoiseGate {
                        threshold_db,
                        reduction_db,
                        attack_ms,
                        hold_ms,
                        release_ms,
                    },
                ) => {
                    g.set(
                        rate,
                        *threshold_db,
                        *reduction_db,
                        *attack_ms,
                        *hold_ms,
                        *release_ms,
                    );
                    true
                }
                (
                    State::DeEss(d),
                    Effect::DeEsser {
                        threshold_db,
                        freq_hz,
                        max_reduction_db,
                    },
                ) => {
                    d.set(
                        rate,
                        self.channels,
                        *threshold_db,
                        *freq_hz,
                        *max_reduction_db,
                    );
                    true
                }
                (
                    State::Reverb(r),
                    Effect::Reverb {
                        room,
                        damping,
                        wet_db,
                        dry_db,
                        width,
                        pre_delay_ms,
                        ..
                    },
                ) => {
                    r.set(
                        rate,
                        *room,
                        *damping,
                        *wet_db,
                        *dry_db,
                        *width,
                        *pre_delay_ms,
                    );
                    true
                }
                (State::Vocal(v), Effect::VocalRemover { keep_bass_hz }) => {
                    v.set(rate, *keep_bass_hz);
                    true
                }
                // Stateless: rebuilding is free.
                _ => false,
            };
        if !kept && let Some(s) = build(fx, rate, self.channels) {
            self.state = s;
        }
        self.effect = *fx;
    }

    pub fn process(&mut self, chans: &mut [Vec<f32>]) {
        match &mut self.state {
            State::Gain(g) => chans.iter_mut().flatten().for_each(|x| *x *= *g),
            State::Filters(per_ch) => {
                for (c, fs) in chans.iter_mut().zip(per_ch.iter_mut()) {
                    for f in fs.iter_mut() {
                        for x in c.iter_mut() {
                            *x = f.tick(*x);
                        }
                    }
                }
            }
            State::Echo { ring, idx, decay } => {
                let n = ring.first().map_or(1, Vec::len);
                let len = chans.first().map_or(0, Vec::len);
                let start = *idx;
                for (c, r) in chans.iter_mut().zip(ring.iter_mut()) {
                    let mut k = start;
                    for x in c.iter_mut().take(len) {
                        let y = *x + r[k] * *decay;
                        r[k] = y;
                        *x = y;
                        k = (k + 1) % n;
                    }
                }
                *idx = (start + len) % n;
            }
            State::Comp(c) => c.process(chans),
            State::Limit(l) => l.process(chans),
            State::Gate(g) => g.process(chans),
            State::DeEss(d) => d.process(chans),
            State::Distort {
                kind,
                drive_db,
                mix,
                output_db,
            } => chans
                .iter_mut()
                .for_each(|c| distort(c, *kind, *drive_db, *mix, *output_db)),
            State::Reverb(r) => r.process(chans),
            State::Widen {
                width,
                mid_db,
                side_db,
            } => {
                stereo_width(chans, *width, *mid_db, *side_db);
            }
            State::Vocal(v) => v.process(chans),
        }
        // Never let a runaway setting (e.g. echo decay > 1) produce inf/NaN.
        for x in chans.iter_mut().flatten() {
            if !x.is_finite() {
                *x = 0.0;
            }
        }
    }
}

enum Slot {
    Builtin(Processor),
    Plugin(u64, SharedProcess),
    /// A plugin that isn't loaded (yet): audio passes through.
    Missing(u64),
}

impl Slot {
    fn id(&self) -> u64 {
        match self {
            Slot::Builtin(p) => p.id,
            Slot::Plugin(id, _) | Slot::Missing(id) => *id,
        }
    }
}

/// A track's rack of running effects.
#[derive(Default)]
pub struct Chain {
    procs: Vec<(Slot, bool)>,
    rate: f32,
    channels: usize,
}

impl Chain {
    pub fn new(specs: &[LiveFx], rate: f32, channels: usize) -> Self {
        Self::with_plugins(specs, rate, channels, &PluginProcs::new())
    }

    pub fn with_plugins(
        specs: &[LiveFx],
        rate: f32,
        channels: usize,
        plugins: &PluginProcs,
    ) -> Self {
        let mut c = Self {
            procs: Vec::new(),
            rate,
            channels,
        };
        c.sync_with(specs, plugins);
        c
    }

    pub fn sync(&mut self, specs: &[LiveFx]) {
        self.sync_with(specs, &PluginProcs::new());
    }

    /// Bring the rack in line with `specs` (matched by id, so a slot keeps its state
    /// when its settings change or the rack is reordered). Plugin slots use the running
    /// processors in `plugins`.
    pub fn sync_with(&mut self, specs: &[LiveFx], plugins: &PluginProcs) {
        let mut old: Vec<(Slot, bool)> = std::mem::take(&mut self.procs);
        for s in specs {
            let prev = old
                .iter()
                .position(|(p, _)| p.id() == s.id)
                .map(|k| old.swap_remove(k).0);
            let slot = if s.plugin.is_some() {
                Some(match plugins.get(&s.id) {
                    Some(p) => Slot::Plugin(s.id, p.clone()),
                    None => Slot::Missing(s.id),
                })
            } else {
                match prev {
                    Some(Slot::Builtin(mut p)) => {
                        p.update(&s.effect);
                        Some(Slot::Builtin(p))
                    }
                    _ => {
                        Processor::new(s.id, &s.effect, self.rate, self.channels).map(Slot::Builtin)
                    }
                }
            };
            if let Some(p) = slot {
                self.procs.push((p, s.enabled));
            }
        }
    }

    pub fn is_active(&self) -> bool {
        self.procs.iter().any(|(_, on)| *on)
    }

    pub fn process(&mut self, chans: &mut [Vec<f32>]) {
        for (p, on) in &mut self.procs {
            if !*on {
                continue;
            }
            match p {
                Slot::Builtin(p) => p.process(chans),
                // Never wait for the UI: if it holds the plugin, skip it for this block.
                Slot::Plugin(_, sp) => {
                    if let Ok(mut g) = sp.try_lock() {
                        g.process(chans);
                    }
                }
                Slot::Missing(_) => {}
            }
        }
    }
}

/// Apply a rack to a whole piece of audio (export, mixdown, "Apply live effects").
/// Plugins are loaded fresh at `rate` with their saved settings; one that fails to load
/// is skipped (and reported on stderr).
pub fn render(specs: &[LiveFx], mut chans: Vec<Vec<f32>>, rate: u32) -> Vec<Vec<f32>> {
    if !specs.iter().any(|s| s.enabled) || chans.is_empty() {
        return chans;
    }
    let len = chans[0].len();
    const BLOCK: usize = 4096;
    let mut block: Vec<Vec<f32>> = vec![Vec::with_capacity(BLOCK); chans.len()];
    for s in specs.iter().filter(|s| s.enabled) {
        if let Some(pl) = &s.plugin {
            if let Err(e) =
                crate::plugins::render_offline(&pl.desc, &pl.state, &mut chans, rate as f64)
            {
                eprintln!("{}: {e:#}", pl.desc.name);
            }
            continue;
        }
        let Some(mut p) = Processor::new(s.id, &s.effect, rate as f32, chans.len()) else {
            continue;
        };
        let mut i = 0;
        while i < len {
            let n = BLOCK.min(len - i);
            for (b, c) in block.iter_mut().zip(&chans) {
                b.clear();
                b.extend_from_slice(&c[i..i + n]);
            }
            p.process(&mut block);
            for (b, c) in block.iter().zip(chans.iter_mut()) {
                c[i..i + n].copy_from_slice(b);
            }
            i += n;
        }
    }
    chans
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(f: f32, n: usize) -> Vec<f32> {
        (0..n)
            .map(|i| (i as f32 * 2.0 * std::f32::consts::PI * f / 48000.0).sin() * 0.5)
            .collect()
    }

    /// Processing in small blocks gives the same result as processing everything at once
    /// (state carries across blocks), and matches the offline effect where it should.
    #[test]
    fn block_size_does_not_matter_and_matches_offline() {
        let all: Vec<Effect> = catalog();
        assert!(all.len() >= 14);
        for fx in all {
            let specs = vec![LiveFx::builtin(1, fx)];
            let input = vec![tone(440.0, 9000), tone(3000.0, 9000)];
            let whole = render(&specs, input.clone(), 48000);
            let mut chain = Chain::new(&specs, 48000.0, 2);
            let mut pieces: Vec<Vec<f32>> = vec![Vec::new(), Vec::new()];
            let mut i = 0;
            for step in [1usize, 7, 64, 256, 1000].iter().cycle() {
                if i >= 9000 {
                    break;
                }
                let n = (*step).min(9000 - i);
                let mut b: Vec<Vec<f32>> = input.iter().map(|c| c[i..i + n].to_vec()).collect();
                chain.process(&mut b);
                for (p, x) in pieces.iter_mut().zip(b) {
                    p.extend(x);
                }
                i += n;
            }
            for c in 0..2 {
                let diff = whole[c]
                    .iter()
                    .zip(&pieces[c])
                    .map(|(a, b)| (a - b).abs())
                    .fold(0f32, f32::max);
                assert!(
                    diff < 1e-5,
                    "{}: block processing differs by {diff}",
                    fx.name()
                );
            }
            // Same-length offline effects agree with the live version (the limiter and reverb
            // differ by design: no look-ahead, and the offline reverb adds a tail).
            if !matches!(fx, Effect::Limiter { .. } | Effect::Reverb { .. }) {
                let off = crate::effects::render(&fx, input.clone(), 48000, None).unwrap();
                for c in 0..2 {
                    let diff = off[c]
                        .iter()
                        .zip(&whole[c])
                        .map(|(a, b)| (a - b).abs())
                        .fold(0f32, f32::max);
                    assert!(
                        diff < 2e-3,
                        "{}: live differs from offline by {diff}",
                        fx.name()
                    );
                }
            }
        }
    }

    #[test]
    fn settings_change_without_reset_and_bypass_works() {
        let eq = |g: f32| Effect::GraphicEq { gains: [g; 10] };
        let mut specs = vec![LiveFx::builtin(7, eq(0.0))];
        let mut chain = Chain::new(&specs, 48000.0, 1);
        let mut b = vec![tone(1000.0, 512)];
        let orig = b.clone();
        chain.process(&mut b);
        assert!(
            b[0].iter().zip(&orig[0]).all(|(a, x)| (a - x).abs() < 1e-4),
            "0 dB EQ is transparent"
        );
        specs[0].effect = eq(6.0);
        chain.sync(&specs);
        let mut b = vec![tone(1000.0, 4800)];
        chain.process(&mut b);
        let peak = b[0][2400..].iter().fold(0f32, |m, x| m.max(x.abs()));
        assert!(peak > 0.8, "boosted: {peak}");
        specs[0].enabled = false;
        chain.sync(&specs);
        assert!(!chain.is_active());
        let mut b = vec![tone(1000.0, 64)];
        let orig = b.clone();
        chain.process(&mut b);
        assert_eq!(b, orig);
        // A runaway echo never produces NaN/inf.
        let wild = vec![LiveFx::builtin(
            2,
            Effect::Echo {
                delay_secs: 0.001,
                decay: 5.0,
            },
        )];
        let out = render(&wild, vec![tone(100.0, 48000)], 48000);
        assert!(out[0].iter().all(|x| x.is_finite()));
    }
}
