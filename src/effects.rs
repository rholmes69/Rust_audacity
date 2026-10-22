//! Effect catalogue: every effect is a serialisable value (so it can live in an
//! effect chain), with defaults, a menu category and a `render` function that
//! turns a block of planar audio into processed audio.

use std::f32::consts::PI;

use serde::{Deserialize, Serialize};

use crate::{
    dsp::{
        analysis,
        biquad::{BUTTERWORTH_Q, Biquad},
        denoise::{self, NoiseProfile},
        dynamics, restore, space, stretch,
        tone::{self, DistortionKind},
    },
    project::{Project, Selection},
    track::Track,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Category {
    Basic,
    Cleanup,
    Dynamics,
    EqTone,
    TimePitch,
    SpaceStereo,
}

impl Category {
    pub const ALL: [Category; 6] = [
        Category::Basic,
        Category::Cleanup,
        Category::Dynamics,
        Category::EqTone,
        Category::TimePitch,
        Category::SpaceStereo,
    ];
    pub fn label(self) -> &'static str {
        match self {
            Category::Basic => "Basic",
            Category::Cleanup => "Cleanup & Restoration",
            Category::Dynamics => "Dynamics",
            Category::EqTone => "EQ & Tone",
            Category::TimePitch => "Time & Pitch",
            Category::SpaceStereo => "Space & Stereo",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub enum Effect {
    // ---- basic
    Amplify {
        db: f32,
    },
    Normalize {
        peak_db: f32,
        remove_dc: bool,
    },
    FadeIn,
    FadeOut,
    Reverse,
    Invert,
    Echo {
        delay_secs: f32,
        decay: f32,
    },
    LowPass {
        cutoff_hz: f32,
    },
    HighPass {
        cutoff_hz: f32,
    },
    // ---- cleanup
    NoiseReduction {
        reduction_db: f32,
        sensitivity_db: f32,
        smoothing: u32,
    },
    ClickRemoval {
        sensitivity: f32,
        max_width_ms: f32,
    },
    HumRemoval {
        base_hz: f32,
        harmonics: u32,
        q: f32,
    },
    DeEsser {
        threshold_db: f32,
        freq_hz: f32,
        max_reduction_db: f32,
    },
    ClipRepair {
        threshold: f32,
        reduce_db: f32,
    },
    // ---- dynamics
    Compressor {
        threshold_db: f32,
        ratio: f32,
        attack_ms: f32,
        release_ms: f32,
        knee_db: f32,
        makeup_db: f32,
    },
    Limiter {
        ceiling_db: f32,
        release_ms: f32,
    },
    NoiseGate {
        threshold_db: f32,
        reduction_db: f32,
        attack_ms: f32,
        hold_ms: f32,
        release_ms: f32,
    },
    LoudnessNormalize {
        target_lufs: f32,
        limit_peaks: bool,
    },
    // ---- EQ & tone
    GraphicEq {
        gains: [f32; 10],
    },
    BassTreble {
        bass_db: f32,
        treble_db: f32,
    },
    Distortion {
        kind: DistortionKind,
        drive_db: f32,
        mix: f32,
        output_db: f32,
    },
    // ---- time & pitch
    ChangeSpeed {
        factor: f32,
    },
    ChangeTempo {
        percent: f32,
    },
    ChangePitch {
        semitones: f32,
    },
    // ---- space & stereo
    Reverb {
        room: f32,
        damping: f32,
        wet_db: f32,
        dry_db: f32,
        width: f32,
        pre_delay_ms: f32,
        tail_secs: f32,
    },
    StereoWidener {
        width: f32,
        mid_db: f32,
        side_db: f32,
    },
    VocalRemover {
        keep_bass_hz: f32,
    },
}

/// How an effect's output relates to its input length.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum LengthPolicy {
    Same,
    /// Output may be longer/shorter; it replaces the selection and later audio shifts.
    Changes,
    /// Output is longer; the extra tail is mixed over the audio after the selection.
    Tail,
}

impl Effect {
    /// One default instance of every effect, in menu order.
    pub fn catalog() -> Vec<Effect> {
        vec![
            Effect::Amplify { db: 0.0 },
            Effect::Normalize {
                peak_db: -1.0,
                remove_dc: true,
            },
            Effect::FadeIn,
            Effect::FadeOut,
            Effect::Reverse,
            Effect::Invert,
            Effect::Echo {
                delay_secs: 0.5,
                decay: 0.5,
            },
            Effect::LowPass { cutoff_hz: 1000.0 },
            Effect::HighPass { cutoff_hz: 80.0 },
            Effect::NoiseReduction {
                reduction_db: 12.0,
                sensitivity_db: 6.0,
                smoothing: 3,
            },
            Effect::ClickRemoval {
                sensitivity: 6.0,
                max_width_ms: 2.0,
            },
            Effect::HumRemoval {
                base_hz: 60.0,
                harmonics: 6,
                q: 30.0,
            },
            Effect::DeEsser {
                threshold_db: -30.0,
                freq_hz: 5500.0,
                max_reduction_db: 10.0,
            },
            Effect::ClipRepair {
                threshold: 0.98,
                reduce_db: 6.0,
            },
            Effect::Compressor {
                threshold_db: -18.0,
                ratio: 3.0,
                attack_ms: 10.0,
                release_ms: 150.0,
                knee_db: 6.0,
                makeup_db: 3.0,
            },
            Effect::Limiter {
                ceiling_db: -1.0,
                release_ms: 60.0,
            },
            Effect::NoiseGate {
                threshold_db: -45.0,
                reduction_db: 30.0,
                attack_ms: 2.0,
                hold_ms: 80.0,
                release_ms: 150.0,
            },
            Effect::LoudnessNormalize {
                target_lufs: -16.0,
                limit_peaks: true,
            },
            Effect::GraphicEq { gains: [0.0; 10] },
            Effect::BassTreble {
                bass_db: 0.0,
                treble_db: 0.0,
            },
            Effect::Distortion {
                kind: DistortionKind::Soft,
                drive_db: 12.0,
                mix: 1.0,
                output_db: -6.0,
            },
            Effect::ChangeSpeed { factor: 1.0 },
            Effect::ChangeTempo { percent: 0.0 },
            Effect::ChangePitch { semitones: 0.0 },
            Effect::Reverb {
                room: 0.6,
                damping: 0.5,
                wet_db: -6.0,
                dry_db: 0.0,
                width: 1.0,
                pre_delay_ms: 10.0,
                tail_secs: 2.0,
            },
            Effect::StereoWidener {
                width: 1.5,
                mid_db: 0.0,
                side_db: 0.0,
            },
            Effect::VocalRemover {
                keep_bass_hz: 150.0,
            },
        ]
    }

    pub fn name(&self) -> &'static str {
        match self {
            Effect::Amplify { .. } => "Amplify",
            Effect::Normalize { .. } => "Normalize",
            Effect::FadeIn => "Fade In",
            Effect::FadeOut => "Fade Out",
            Effect::Reverse => "Reverse",
            Effect::Invert => "Invert",
            Effect::Echo { .. } => "Echo",
            Effect::LowPass { .. } => "Low-Pass Filter",
            Effect::HighPass { .. } => "High-Pass Filter",
            Effect::NoiseReduction { .. } => "Noise Reduction",
            Effect::ClickRemoval { .. } => "Click & Pop Removal",
            Effect::HumRemoval { .. } => "Hum Removal",
            Effect::DeEsser { .. } => "De-Esser",
            Effect::ClipRepair { .. } => "Clip Repair",
            Effect::Compressor { .. } => "Compressor",
            Effect::Limiter { .. } => "Limiter",
            Effect::NoiseGate { .. } => "Noise Gate",
            Effect::LoudnessNormalize { .. } => "Loudness Normalize (LUFS)",
            Effect::GraphicEq { .. } => "Graphic EQ",
            Effect::BassTreble { .. } => "Bass & Treble",
            Effect::Distortion { .. } => "Distortion",
            Effect::ChangeSpeed { .. } => "Change Speed",
            Effect::ChangeTempo { .. } => "Change Tempo",
            Effect::ChangePitch { .. } => "Change Pitch",
            Effect::Reverb { .. } => "Reverb",
            Effect::StereoWidener { .. } => "Stereo Widener",
            Effect::VocalRemover { .. } => "Vocal Remover (Centre Cancel)",
        }
    }

    pub fn category(&self) -> Category {
        use Effect::*;
        match self {
            Amplify { .. }
            | Normalize { .. }
            | FadeIn
            | FadeOut
            | Reverse
            | Invert
            | Echo { .. }
            | LowPass { .. }
            | HighPass { .. } => Category::Basic,
            NoiseReduction { .. }
            | ClickRemoval { .. }
            | HumRemoval { .. }
            | DeEsser { .. }
            | ClipRepair { .. } => Category::Cleanup,
            Compressor { .. } | Limiter { .. } | NoiseGate { .. } | LoudnessNormalize { .. } => {
                Category::Dynamics
            }
            GraphicEq { .. } | BassTreble { .. } | Distortion { .. } => Category::EqTone,
            ChangeSpeed { .. } | ChangeTempo { .. } | ChangePitch { .. } => Category::TimePitch,
            Reverb { .. } | StereoWidener { .. } | VocalRemover { .. } => Category::SpaceStereo,
        }
    }

    /// Effects without parameters apply immediately from the menu.
    pub fn has_params(&self) -> bool {
        !matches!(
            self,
            Effect::FadeIn | Effect::FadeOut | Effect::Reverse | Effect::Invert
        )
    }

    pub fn length_policy(&self) -> LengthPolicy {
        match self {
            Effect::ChangeSpeed { .. } | Effect::ChangeTempo { .. } => LengthPolicy::Changes,
            Effect::Reverb { tail_secs, .. } if *tail_secs > 0.0 => LengthPolicy::Tail,
            _ => LengthPolicy::Same,
        }
    }

    pub fn needs_stereo(&self) -> bool {
        matches!(
            self,
            Effect::StereoWidener { .. } | Effect::VocalRemover { .. }
        )
    }

    pub fn help(&self) -> &'static str {
        match self {
            Effect::NoiseReduction { .. } => {
                "Step 1: select a few seconds of noise only and click “Get Noise Profile”.\nStep 2: select the audio to clean (or nothing for the whole track) and click OK."
            }
            Effect::ClickRemoval { .. } => {
                "Finds short clicks, pops and crackle and rebuilds the waveform across them."
            }
            Effect::HumRemoval { .. } => {
                "Removes electrical mains hum: 60 Hz (Americas) or 50 Hz (Europe/Asia) plus harmonics."
            }
            Effect::DeEsser { .. } => "Turns down harsh “s” and “sh” sounds only when they happen.",
            Effect::ClipRepair { .. } => {
                "Rebuilds the flattened tops of clipped peaks, then lowers the level so they fit."
            }
            Effect::Compressor { .. } => {
                "Evens out loud and quiet parts. Gain reduction starts at the threshold."
            }
            Effect::Limiter { .. } => {
                "Stops any peak from going above the ceiling (look-ahead, no overs)."
            }
            Effect::NoiseGate { .. } => {
                "Silences the gaps between phrases when the level drops below the threshold."
            }
            Effect::LoudnessNormalize { .. } => {
                "Matches perceived loudness: −14 LUFS streaming, −16 podcasts, −23 broadcast."
            }
            Effect::GraphicEq { .. } => "10-band equalizer. Drag the sliders or pick a preset.",
            Effect::BassTreble { .. } => "Shelving tone controls around 250 Hz and 4 kHz.",
            Effect::Distortion { .. } => "Adds saturation or overdrive.",
            Effect::ChangeSpeed { .. } => "Changes speed and pitch together, like a tape machine.",
            Effect::ChangeTempo { .. } => "Changes speed without changing pitch.",
            Effect::ChangePitch { .. } => "Changes pitch without changing speed.",
            Effect::Reverb { .. } => "Adds room ambience. The tail rings out over following audio.",
            Effect::StereoWidener { .. } => {
                "Mid/side processing: 0 = mono, 1 = unchanged, 2 = extra wide. Stereo tracks only."
            }
            Effect::VocalRemover { .. } => {
                "Cancels centre-panned audio (usually the lead vocal). Keeps bass below the cutoff. Stereo tracks only."
            }
            _ => "",
        }
    }
}

pub fn db_to_lin(db: f32) -> f32 {
    10f32.powf(db / 20.0)
}

pub fn lin_to_db(lin: f32) -> f32 {
    if lin <= 0.0 {
        f32::NEG_INFINITY
    } else {
        20.0 * lin.log10()
    }
}

/// Peak absolute amplitude across target tracks within the effect range.
pub fn selection_peak(p: &Project) -> f32 {
    let (s, e) = p.effect_range();
    let mut peak = 0f32;
    for i in p.target_tracks() {
        let t = &p.tracks[i];
        let e = e.min(t.len());
        if s >= e {
            continue;
        }
        for ch in &t.channels {
            for &x in &ch[s..e] {
                peak = peak.max(x.abs());
            }
        }
    }
    peak
}

/// Process one block of planar audio. Returns the processed block (possibly a different length).
pub fn render(
    fx: &Effect,
    mut data: Vec<Vec<f32>>,
    rate: u32,
    profile: Option<&NoiseProfile>,
) -> Result<Vec<Vec<f32>>, String> {
    let r = rate as f32;
    let len = data.first().map_or(0, Vec::len);
    if len == 0 {
        return Ok(data);
    }
    let each = |data: &mut Vec<Vec<f32>>, f: &mut dyn FnMut(&mut [f32])| {
        data.iter_mut().for_each(|c| f(c))
    };
    match *fx {
        Effect::Amplify { db } => {
            let g = db_to_lin(db);
            each(&mut data, &mut |c| c.iter_mut().for_each(|x| *x *= g));
        }
        Effect::Normalize { peak_db, remove_dc } => {
            if remove_dc {
                each(&mut data, &mut |c| {
                    let mean = c.iter().map(|&x| x as f64).sum::<f64>() / c.len() as f64;
                    c.iter_mut().for_each(|x| *x -= mean as f32);
                });
            }
            let peak = data.iter().flatten().fold(0f32, |m, x| m.max(x.abs()));
            if peak > 0.0 {
                let g = db_to_lin(peak_db) / peak;
                each(&mut data, &mut |c| c.iter_mut().for_each(|x| *x *= g));
            }
        }
        Effect::FadeIn => {
            let d = (len.max(2) - 1) as f32;
            each(&mut data, &mut |c| {
                c.iter_mut()
                    .enumerate()
                    .for_each(|(i, x)| *x *= i as f32 / d)
            });
        }
        Effect::FadeOut => {
            let d = (len.max(2) - 1) as f32;
            each(&mut data, &mut |c| {
                c.iter_mut()
                    .enumerate()
                    .for_each(|(i, x)| *x *= 1.0 - i as f32 / d)
            });
        }
        Effect::Reverse => each(&mut data, &mut |c| c.reverse()),
        Effect::Invert => each(&mut data, &mut |c| c.iter_mut().for_each(|x| *x = -*x)),
        Effect::Echo { delay_secs, decay } => {
            let d = (delay_secs * r).round() as usize;
            if d > 0 {
                each(&mut data, &mut |c| {
                    for i in d..c.len() {
                        c[i] += c[i - d] * decay;
                    }
                });
            }
        }
        Effect::LowPass { cutoff_hz } => each(&mut data, &mut |c| {
            Biquad::lowpass(cutoff_hz, BUTTERWORTH_Q, r).run(c)
        }),
        Effect::HighPass { cutoff_hz } => each(&mut data, &mut |c| {
            Biquad::highpass(cutoff_hz, BUTTERWORTH_Q, r).run(c)
        }),
        Effect::NoiseReduction {
            reduction_db,
            sensitivity_db,
            smoothing,
        } => {
            let profile = profile.ok_or("No noise profile yet. Select some noise-only audio and click “Get Noise Profile” first.")?;
            if profile.rate != rate {
                return Err(format!(
                    "The noise profile was taken at {} Hz but this project is {} Hz. Take a new profile.",
                    profile.rate, rate
                ));
            }
            for c in data.iter_mut() {
                *c = denoise::reduce(
                    c,
                    profile,
                    reduction_db,
                    sensitivity_db,
                    smoothing as usize,
                    rate,
                );
            }
        }
        Effect::ClickRemoval {
            sensitivity,
            max_width_ms,
        } => each(&mut data, &mut |c| {
            restore::remove_clicks(c, r, sensitivity, max_width_ms);
        }),
        Effect::HumRemoval {
            base_hz,
            harmonics,
            q,
        } => each(&mut data, &mut |c| {
            restore::remove_hum(c, r, base_hz, harmonics as usize, q)
        }),
        Effect::DeEsser {
            threshold_db,
            freq_hz,
            max_reduction_db,
        } => dynamics::de_ess(&mut data, r, threshold_db, freq_hz, max_reduction_db),
        Effect::ClipRepair {
            threshold,
            reduce_db,
        } => each(&mut data, &mut |c| {
            restore::repair_clipping(c, threshold, reduce_db);
        }),
        Effect::Compressor {
            threshold_db,
            ratio,
            attack_ms,
            release_ms,
            knee_db,
            makeup_db,
        } => dynamics::compress(
            &mut data,
            r,
            threshold_db,
            ratio,
            attack_ms,
            release_ms,
            knee_db,
            makeup_db,
        ),
        Effect::Limiter {
            ceiling_db,
            release_ms,
        } => dynamics::limit(&mut data, r, ceiling_db, release_ms),
        Effect::NoiseGate {
            threshold_db,
            reduction_db,
            attack_ms,
            hold_ms,
            release_ms,
        } => dynamics::gate(
            &mut data,
            r,
            threshold_db,
            reduction_db,
            attack_ms,
            hold_ms,
            release_ms,
        ),
        Effect::LoudnessNormalize {
            target_lufs,
            limit_peaks,
        } => {
            let current = analysis::integrated_lufs(&data, rate).ok_or("Audio is too short or too quiet to measure loudness (needs at least 0.4 s of sound).")?;
            let g = db_to_lin(target_lufs - current as f32);
            each(&mut data, &mut |c| c.iter_mut().for_each(|x| *x *= g));
            if limit_peaks {
                dynamics::limit(&mut data, r, -1.0, 60.0);
            }
        }
        Effect::GraphicEq { gains } => each(&mut data, &mut |c| tone::graphic_eq(c, &gains, r)),
        Effect::BassTreble { bass_db, treble_db } => each(&mut data, &mut |c| {
            tone::bass_treble(c, bass_db, treble_db, r)
        }),
        Effect::Distortion {
            kind,
            drive_db,
            mix,
            output_db,
        } => each(&mut data, &mut |c| {
            tone::distort(c, kind, drive_db, mix, output_db)
        }),
        Effect::ChangeSpeed { factor } => {
            if factor > 0.0 && (factor - 1.0).abs() > 1e-6 {
                let out_len = ((len as f64) / factor as f64).round().max(1.0) as usize;
                data = data
                    .iter()
                    .map(|c| crate::track::resample_ratio(c, factor as f64, out_len))
                    .collect();
            }
        }
        Effect::ChangeTempo { percent } => {
            let tempo = 1.0 + percent as f64 / 100.0;
            if tempo <= 0.05 {
                return Err("Tempo change is too large.".into());
            }
            data = stretch::time_stretch(&data, tempo, rate);
        }
        Effect::ChangePitch { semitones } => {
            data = stretch::pitch_shift(&data, semitones as f64, rate)
        }
        Effect::Reverb {
            room,
            damping,
            wet_db,
            dry_db,
            width,
            pre_delay_ms,
            tail_secs,
        } => {
            let tail = (tail_secs.max(0.0) * r) as usize;
            data = space::reverb(
                &data,
                r,
                room,
                damping,
                wet_db,
                dry_db,
                width,
                pre_delay_ms,
                tail,
            );
        }
        Effect::StereoWidener {
            width,
            mid_db,
            side_db,
        } => {
            space::stereo_width(&mut data, width, mid_db, side_db);
        }
        Effect::VocalRemover { keep_bass_hz } => {
            space::vocal_remover(&mut data, r, keep_bass_hz);
        }
    }
    Ok(data)
}

/// Apply one effect to a single track's [start, end) range. Returns the new range length.
fn apply_to_track(
    t: &mut Track,
    fx: &Effect,
    start: usize,
    end: usize,
    rate: u32,
    profile: Option<&NoiseProfile>,
) -> Result<usize, String> {
    let e = end.min(t.len());
    if start >= e {
        return Ok(0);
    }
    if fx.needs_stereo() && t.num_channels() < 2 {
        return Ok(e - start);
    }
    let input = t.extract(start, e);
    let out = render(fx, input, rate, profile)?;
    let new_len = out.first().map_or(0, Vec::len);
    match fx.length_policy() {
        LengthPolicy::Tail if new_len > e - start => {
            let body: Vec<Vec<f32>> = out.iter().map(|c| c[..e - start].to_vec()).collect();
            t.replace(start, e, &body);
            // Mix the tail over what follows.
            let tail_len = new_len - (e - start);
            t.pad_to(e + tail_len);
            let tails: Vec<Vec<f32>> = out.iter().map(|c| c[e - start..].to_vec()).collect();
            let mut ci = 0;
            t.process(e, e + tail_len, |buf| {
                for (b, s) in buf.iter_mut().zip(&tails[ci.min(tails.len() - 1)]) {
                    *b += s;
                }
                ci += 1;
            });
            Ok(e - start)
        }
        LengthPolicy::Same => {
            t.replace(start, e, &out);
            Ok(e - start)
        }
        _ => {
            t.replace(start, e, &out);
            Ok(new_len)
        }
    }
}

fn check(p: &Project) -> Result<(), String> {
    if p.tracks.is_empty() {
        return Err("There is no audio to process.".into());
    }
    let (start, end) = p.effect_range();
    if start >= end {
        return Err("Select some audio first.".into());
    }
    Ok(())
}

/// Error returned by [`EffectJob::run`] when the progress callback asks to stop.
pub const CANCELLED: &str = "Cancelled";

/// A self-contained effect (or chain) job.
///
/// The target tracks are cloned out of the project (cheap: channels are `Arc`s and only
/// copied when written), processed — possibly on another thread — and the finished tracks
/// are swapped back in with [`JobResult::commit`] as one undo step.
pub struct EffectJob {
    pub label: String,
    chain: Vec<Effect>,
    tracks: Vec<Track>,
    start: usize,
    end: usize,
    rate: u32,
    profile: Option<NoiseProfile>,
}

/// The output of a finished [`EffectJob`].
pub struct JobResult {
    pub label: String,
    tracks: Vec<Track>,
    selection: Option<Selection>,
    /// Range the job processed, and whether it stretched/shrank audio inside it.
    start: usize,
    end: usize,
    changes_length: bool,
}

impl EffectJob {
    /// Capture everything needed to run `chain` on the project's current selection.
    pub fn prepare(
        p: &Project,
        label: impl Into<String>,
        chain: Vec<Effect>,
    ) -> Result<Self, String> {
        check(p)?;
        if chain.is_empty() {
            return Err("The chain is empty.".into());
        }
        let (start, end) = p.effect_range();
        Ok(Self {
            label: label.into(),
            tracks: p
                .target_tracks()
                .into_iter()
                .map(|i| p.tracks[i].clone())
                .collect(),
            chain,
            start,
            end,
            rate: p.rate,
            profile: p.noise_profile.clone(),
        })
    }

    /// Number of progress steps (one per track per effect).
    pub fn steps(&self) -> usize {
        (self.tracks.len() * self.chain.len()).max(1)
    }

    /// Length of audio being processed, in samples (per track).
    pub fn range_len(&self) -> usize {
        self.end - self.start
    }

    /// Process the tracks. `progress(done, total)` is called after every step; returning
    /// `false` stops the job with [`CANCELLED`].
    pub fn run(
        mut self,
        mut progress: impl FnMut(usize, usize) -> bool,
    ) -> Result<JobResult, String> {
        let total = self.steps();
        let mut done = 0;
        let mut end = self.end;
        let mut selection = None;
        let multi = self.chain.len() > 1;
        for fx in &self.chain {
            let mut longest = 0;
            let mut skipped_mono = 0;
            for t in &mut self.tracks {
                if fx.needs_stereo() && t.num_channels() < 2 {
                    skipped_mono += 1;
                } else {
                    let n =
                        apply_to_track(t, fx, self.start, end, self.rate, self.profile.as_ref())
                            .map_err(|e| {
                                if multi {
                                    format!("{}: {e}", fx.name())
                                } else {
                                    e
                                }
                            })?;
                    longest = longest.max(n);
                }
                done += 1;
                if !progress(done, total) {
                    return Err(CANCELLED.into());
                }
            }
            if skipped_mono > 0 && longest == 0 {
                return Err(format!("{} only works on stereo tracks.", fx.name()));
            }
            if fx.length_policy() == LengthPolicy::Changes {
                end = self.start + longest;
                selection = Some(Selection::new(self.start, end));
            }
        }
        let changes_length = self
            .chain
            .iter()
            .any(|fx| fx.length_policy() == LengthPolicy::Changes);
        Ok(JobResult {
            label: self.label,
            tracks: self.tracks,
            selection,
            start: self.start,
            end: self.end,
            changes_length,
        })
    }
}

impl JobResult {
    /// Swap the processed tracks into the project as one undo step. Tracks that were
    /// removed while the job ran are skipped. Returns false if nothing could be applied.
    pub fn commit(self, p: &mut Project) -> bool {
        if !self
            .tracks
            .iter()
            .any(|t| p.tracks.iter().any(|o| o.id == t.id))
        {
            return false;
        }
        p.push_undo(self.label);
        for t in self.tracks {
            if let Some(idx) = p.tracks.iter().position(|o| o.id == t.id) {
                let (old_len, new_len) = (p.tracks[idx].len(), t.len());
                // Keep live UI state (mute/solo/gain/selection/height) as it is now.
                p.tracks[idx].channels = t.channels;
                if self.changes_length && new_len != old_len {
                    let e = self.end.min(old_len);
                    let ne = (e as i64 + new_len as i64 - old_len as i64).max(self.start as i64);
                    p.retime_transcript(idx, self.start, e, ne as usize);
                }
            }
        }
        if let Some(sel) = self.selection {
            p.selection = sel;
        }
        true
    }
}

/// Apply an effect to the target tracks over the selection (or the whole project).
/// Creates one undo step; on error the project is left unchanged.
pub fn apply(p: &mut Project, fx: &Effect) -> Result<(), String> {
    let job = EffectJob::prepare(p, fx.name(), vec![*fx])?;
    job.run(|_, _| true)?.commit(p);
    Ok(())
}

/// Apply several effects in sequence as a single undo step.
pub fn apply_chain(p: &mut Project, name: &str, chain: &[Effect]) -> Result<(), String> {
    let job = EffectJob::prepare(p, name, chain.to_vec())?;
    job.run(|_, _| true)?.commit(p);
    Ok(())
}

/// Render up to `max_secs` of the selection through `chain` for previewing.
/// Returns a temporary project containing only the processed target tracks.
pub fn preview(p: &Project, chain: &[Effect], max_secs: f32) -> Result<Project, String> {
    check(p)?;
    let (start, end) = p.effect_range();
    let end = end.min(start + (max_secs * p.rate as f32) as usize);
    let mut out = Project::default();
    out.rate = p.rate;
    for ti in p.target_tracks() {
        let t = &p.tracks[ti];
        let e = end.min(t.len());
        if start >= e {
            continue;
        }
        let mut data = t.extract(start, e);
        for fx in chain {
            if fx.needs_stereo() && data.len() < 2 {
                continue;
            }
            data = render(fx, data, p.rate, p.noise_profile.as_ref())?;
        }
        let idx = out.add_track(t.name.clone(), data);
        let nt = &mut out.tracks[idx];
        (nt.gain, nt.pan, nt.mute, nt.solo) = (t.gain, t.pan, t.mute, t.solo);
    }
    Ok(out)
}

// ----- effect chains --------------------------------------------------------

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct EffectChain {
    pub name: String,
    pub steps: Vec<Effect>,
}

/// Ready-made chains that ship with the app.
pub fn builtin_chains() -> Vec<EffectChain> {
    vec![
        EffectChain {
            name: "Podcast Voice".into(),
            steps: vec![
                Effect::HighPass { cutoff_hz: 80.0 },
                Effect::NoiseGate {
                    threshold_db: -50.0,
                    reduction_db: 20.0,
                    attack_ms: 2.0,
                    hold_ms: 120.0,
                    release_ms: 200.0,
                },
                Effect::DeEsser {
                    threshold_db: -28.0,
                    freq_hz: 5500.0,
                    max_reduction_db: 8.0,
                },
                Effect::Compressor {
                    threshold_db: -20.0,
                    ratio: 3.0,
                    attack_ms: 8.0,
                    release_ms: 150.0,
                    knee_db: 6.0,
                    makeup_db: 4.0,
                },
                Effect::LoudnessNormalize {
                    target_lufs: -16.0,
                    limit_peaks: true,
                },
            ],
        },
        EffectChain {
            name: "Clean Up Voice Recording".into(),
            steps: vec![
                Effect::HumRemoval {
                    base_hz: 60.0,
                    harmonics: 6,
                    q: 30.0,
                },
                Effect::ClickRemoval {
                    sensitivity: 6.0,
                    max_width_ms: 2.0,
                },
                Effect::HighPass { cutoff_hz: 70.0 },
                Effect::GraphicEq {
                    gains: [-4.0, -3.0, -1.0, 0.0, 0.0, 1.0, 2.0, 2.0, 1.0, 0.0],
                },
                Effect::Normalize {
                    peak_db: -1.0,
                    remove_dc: true,
                },
            ],
        },
        EffectChain {
            name: "Music Master (Streaming −14 LUFS)".into(),
            steps: vec![
                Effect::HighPass { cutoff_hz: 25.0 },
                Effect::Compressor {
                    threshold_db: -14.0,
                    ratio: 2.0,
                    attack_ms: 25.0,
                    release_ms: 250.0,
                    knee_db: 8.0,
                    makeup_db: 1.0,
                },
                Effect::LoudnessNormalize {
                    target_lufs: -14.0,
                    limit_peaks: true,
                },
            ],
        },
        EffectChain {
            name: "Vinyl Restoration".into(),
            steps: vec![
                Effect::ClickRemoval {
                    sensitivity: 7.0,
                    max_width_ms: 3.0,
                },
                Effect::HumRemoval {
                    base_hz: 60.0,
                    harmonics: 4,
                    q: 25.0,
                },
                Effect::HighPass { cutoff_hz: 30.0 },
                Effect::Normalize {
                    peak_db: -1.0,
                    remove_dc: true,
                },
            ],
        },
        EffectChain {
            name: "Karaoke (remove vocals)".into(),
            steps: vec![
                Effect::VocalRemover {
                    keep_bass_hz: 150.0,
                },
                Effect::LoudnessNormalize {
                    target_lufs: -14.0,
                    limit_peaks: true,
                },
            ],
        },
        EffectChain {
            name: "Lo-Fi Radio".into(),
            steps: vec![
                Effect::HighPass { cutoff_hz: 300.0 },
                Effect::LowPass { cutoff_hz: 3400.0 },
                Effect::Distortion {
                    kind: DistortionKind::Soft,
                    drive_db: 9.0,
                    mix: 0.6,
                    output_db: -3.0,
                },
                Effect::Normalize {
                    peak_db: -1.0,
                    remove_dc: false,
                },
            ],
        },
    ]
}

// ----- generators -----------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Waveform {
    Sine,
    Square,
    Sawtooth,
    Triangle,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NoiseKind {
    White,
    Pink,
    Brown,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Generator {
    Tone {
        wave: Waveform,
        freq: f32,
        amp: f32,
        secs: f32,
    },
    Noise {
        kind: NoiseKind,
        amp: f32,
        secs: f32,
    },
    Silence {
        secs: f32,
    },
}

impl Generator {
    pub fn name(&self) -> &'static str {
        match self {
            Generator::Tone { .. } => "Tone",
            Generator::Noise { .. } => "Noise",
            Generator::Silence { .. } => "Silence",
        }
    }

    pub fn render(&self, rate: u32) -> Vec<f32> {
        let secs = match *self {
            Generator::Tone { secs, .. }
            | Generator::Noise { secs, .. }
            | Generator::Silence { secs } => secs,
        };
        let n = (secs.max(0.0) * rate as f32).round() as usize;
        match *self {
            Generator::Silence { .. } => vec![0.0; n],
            Generator::Tone {
                wave, freq, amp, ..
            } => (0..n)
                .map(|i| {
                    let ph = (i as f64 * freq as f64 / rate as f64).fract() as f32;
                    amp * match wave {
                        Waveform::Sine => (2.0 * PI * ph).sin(),
                        Waveform::Square => {
                            if ph < 0.5 {
                                1.0
                            } else {
                                -1.0
                            }
                        }
                        Waveform::Sawtooth => 2.0 * ph - 1.0,
                        Waveform::Triangle => 1.0 - 4.0 * (ph - 0.5).abs(),
                    }
                })
                .collect(),
            Generator::Noise { kind, amp, .. } => {
                let mut rng = XorShift(0x9E37_79B9_7F4A_7C15);
                let mut b = [0f32; 7];
                let mut brown = 0f32;
                (0..n)
                    .map(|_| {
                        let w = rng.next_f32();
                        amp * match kind {
                            NoiseKind::White => w,
                            NoiseKind::Pink => {
                                // Paul Kellet's refined pink-noise filter.
                                b[0] = 0.99886 * b[0] + w * 0.055_517_9;
                                b[1] = 0.99332 * b[1] + w * 0.075_075_9;
                                b[2] = 0.96900 * b[2] + w * 0.153_852;
                                b[3] = 0.86650 * b[3] + w * 0.310_485_6;
                                b[4] = 0.55000 * b[4] + w * 0.532_952_2;
                                b[5] = -0.7616 * b[5] - w * 0.016_898;
                                let out = b.iter().sum::<f32>() + w * 0.5362;
                                b[6] = w * 0.115_926;
                                out * 0.11
                            }
                            NoiseKind::Brown => {
                                brown = (brown + 0.02 * w) / 1.02;
                                brown * 3.5
                            }
                        }
                    })
                    .collect()
            }
        }
    }
}

struct XorShift(u64);
impl XorShift {
    fn next_f32(&mut self) -> f32 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        ((x >> 40) as f32 / (1u64 << 24) as f32) * 2.0 - 1.0
    }
}

/// Insert generated audio at the cursor (replacing any selection) in target tracks.
/// Creates a new mono track if the project is empty.
pub fn generate(p: &mut Project, g: Generator) {
    p.push_undo(format!("Generate {}", g.name()));
    let samples = g.render(p.rate);
    let len = samples.len();
    if p.tracks.is_empty() {
        p.add_track(g.name(), vec![samples]);
        p.selection = Selection::new(0, len);
        return;
    }
    let Selection { start, end } = p.selection;
    for i in p.target_tracks() {
        let n = p.tracks[i].num_channels();
        p.tracks[i].replace(start, end, &vec![samples.clone(); n]);
    }
    p.selection = Selection::new(start, start + len);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn proj(data: Vec<f32>) -> Project {
        let mut p = Project::default();
        p.add_track("t", vec![data]);
        p
    }

    #[test]
    fn background_job_runs_off_thread_and_commits_once() {
        let mut p = proj(vec![0.25; 1000]);
        p.add_track("other", vec![vec![0.1; 1000]]);
        p.tracks[1].selected = false;
        let job = EffectJob::prepare(
            &p,
            "Louder then faster",
            vec![
                Effect::Amplify { db: 6.0206 },
                Effect::ChangeSpeed { factor: 2.0 },
            ],
        )
        .unwrap();
        assert_eq!(job.steps(), 2);
        let mut calls = Vec::new();
        let result = std::thread::spawn(move || {
            let r = job.run(|d, t| {
                calls.push((d, t));
                true
            });
            (r, calls)
        });
        // The project can still be edited while the job runs.
        p.tracks[1].gain = 0.5;
        let (r, calls) = result.join().unwrap();
        assert_eq!(calls, vec![(1, 2), (2, 2)]);
        assert!(r.unwrap().commit(&mut p));
        assert_eq!(p.tracks[0].len(), 500);
        assert!((p.tracks[0].channels[0][10] - 0.5).abs() < 1e-4);
        assert_eq!(p.selection, Selection::new(0, 500));
        assert_eq!(p.tracks[1].len(), 1000, "unselected track untouched");
        assert_eq!(p.tracks[1].gain, 0.5);
        assert_eq!(p.undo_label(), Some("Louder then faster"));
        assert!(p.undo());
        assert_eq!(p.tracks[0].len(), 1000);
    }

    #[test]
    fn cancelled_job_changes_nothing() {
        let p = proj(vec![0.25; 100]);
        let job = EffectJob::prepare(&p, "x", vec![Effect::Reverse, Effect::Invert]).unwrap();
        let r = job.run(|_, _| false);
        assert_eq!(r.err().as_deref(), Some(CANCELLED));
        assert!(!p.can_undo());
    }

    #[test]
    fn job_result_for_removed_track_is_dropped() {
        let mut p = proj(vec![0.25; 100]);
        let r = EffectJob::prepare(&p, "x", vec![Effect::Invert])
            .unwrap()
            .run(|_, _| true)
            .unwrap();
        p.tracks.clear();
        assert!(!r.commit(&mut p));
        assert!(!p.can_undo());
    }

    #[test]
    fn amplify_6db_doubles() {
        let mut p = proj(vec![0.25; 4]);
        apply(&mut p, &Effect::Amplify { db: 6.0206 }).unwrap();
        assert!((p.tracks[0].channels[0][0] - 0.5).abs() < 1e-3);
    }

    #[test]
    fn normalize_hits_target() {
        let mut p = proj(vec![0.1, -0.2, 0.05]);
        apply(
            &mut p,
            &Effect::Normalize {
                peak_db: 0.0,
                remove_dc: false,
            },
        )
        .unwrap();
        assert!((selection_peak(&p) - 1.0).abs() < 1e-5);
    }

    #[test]
    fn fades_and_reverse() {
        let mut p = proj(vec![1.0; 5]);
        apply(&mut p, &Effect::FadeIn).unwrap();
        assert_eq!(p.tracks[0].channels[0][0], 0.0);
        assert_eq!(p.tracks[0].channels[0][4], 1.0);
        apply(&mut p, &Effect::Reverse).unwrap();
        assert_eq!(p.tracks[0].channels[0][0], 1.0);
    }

    #[test]
    fn selection_limits_effect() {
        let mut p = proj(vec![1.0; 6]);
        p.selection = Selection::new(2, 4);
        apply(&mut p, &Effect::Invert).unwrap();
        assert_eq!(
            *p.tracks[0].channels[0],
            vec![1.0, 1.0, -1.0, -1.0, 1.0, 1.0]
        );
    }

    #[test]
    fn speed_halves_length() {
        let mut p = proj(vec![0.0; 1000]);
        apply(&mut p, &Effect::ChangeSpeed { factor: 2.0 }).unwrap();
        assert_eq!(p.tracks[0].len(), 500);
    }

    #[test]
    fn failed_effect_leaves_project_untouched() {
        let mut p = proj(vec![0.1; 1000]);
        let err = apply(
            &mut p,
            &Effect::NoiseReduction {
                reduction_db: 12.0,
                sensitivity_db: 6.0,
                smoothing: 3,
            },
        );
        assert!(err.is_err());
        assert!(!p.can_undo());
        assert_eq!(p.tracks[0].channels[0][0], 0.1);
    }

    #[test]
    fn reverb_tail_mixes_into_following_audio() {
        let mut data = vec![0.0; 44100];
        data[0] = 1.0;
        let mut p = proj(data);
        p.selection = Selection::new(0, 100);
        apply(
            &mut p,
            &Effect::Reverb {
                room: 0.8,
                damping: 0.3,
                wet_db: 0.0,
                dry_db: 0.0,
                width: 1.0,
                pre_delay_ms: 0.0,
                tail_secs: 0.5,
            },
        )
        .unwrap();
        assert_eq!(p.tracks[0].len(), 44100);
        let tail_energy: f32 = p.tracks[0].channels[0][2000..20000]
            .iter()
            .map(|x| x * x)
            .sum();
        assert!(tail_energy > 1e-5);
    }

    #[test]
    fn chain_is_one_undo_step() {
        let mut p = proj(vec![0.25; 100]);
        apply_chain(
            &mut p,
            "Test",
            &[Effect::Amplify { db: 6.0206 }, Effect::Invert],
        )
        .unwrap();
        assert!((p.tracks[0].channels[0][0] + 0.5).abs() < 1e-3);
        p.undo();
        assert_eq!(p.tracks[0].channels[0][0], 0.25);
        assert!(!p.can_undo());
    }

    #[test]
    fn every_effect_renders() {
        let rate = 44100;
        let l: Vec<f32> = (0..rate as usize)
            .map(|i| (i as f32 * 0.03).sin() * 0.5)
            .collect();
        let r: Vec<f32> = l.iter().map(|x| x * 0.7).collect();
        let profile = denoise::learn_profile(&[&l[..8192]], rate).unwrap();
        for fx in Effect::catalog() {
            let out = render(&fx, vec![l.clone(), r.clone()], rate, Some(&profile))
                .unwrap_or_else(|e| panic!("{}: {e}", fx.name()));
            assert_eq!(out.len(), 2, "{}", fx.name());
            assert!(
                out[0].iter().all(|x| x.is_finite()),
                "{} produced NaN",
                fx.name()
            );
        }
    }

    #[test]
    fn generate_tone_into_empty_project() {
        let mut p = Project::default();
        generate(
            &mut p,
            Generator::Tone {
                wave: Waveform::Sine,
                freq: 440.0,
                amp: 0.8,
                secs: 1.0,
            },
        );
        assert_eq!(p.tracks.len(), 1);
        assert_eq!(p.tracks[0].len(), 44100);
        assert!(selection_peak(&p) <= 0.8 + 1e-6);
    }
}
