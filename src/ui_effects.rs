//! Parameter editors for every effect (used by effect dialogs and the chain editor).

use eframe::egui::{
    self, Color32, CornerRadius, DragValue, Pos2, Sense, Slider, Stroke, Ui, pos2, vec2,
};

use crate::{
    dsp::tone::{self, DistortionKind, EQ_BANDS, EQ_PRESETS},
    effects::Effect,
};

fn row(ui: &mut Ui, label: &str, add: impl FnOnce(&mut Ui)) {
    ui.label(label);
    add(ui);
    ui.end_row();
}

fn db_drag(v: &mut f32, lo: f32, hi: f32) -> DragValue<'_> {
    DragValue::new(v)
        .speed(0.1)
        .range(lo..=hi)
        .suffix(" dB")
        .max_decimals(1)
}

fn ms_drag(v: &mut f32, lo: f32, hi: f32) -> DragValue<'_> {
    DragValue::new(v)
        .speed(0.5)
        .range(lo..=hi)
        .suffix(" ms")
        .max_decimals(1)
}

fn hz_drag(v: &mut f32, lo: f32, hi: f32) -> DragValue<'_> {
    DragValue::new(v)
        .speed(5.0)
        .range(lo..=hi)
        .suffix(" Hz")
        .max_decimals(0)
}

/// Draw the editor for `fx`. `id` keeps widget ids unique when several editors are on screen.
pub fn params_ui(
    ui: &mut Ui,
    id: impl std::hash::Hash + std::fmt::Debug,
    fx: &mut Effect,
    rate: u32,
) {
    let nyq = rate as f32 * 0.49;
    if let Effect::GraphicEq { gains } = fx {
        graphic_eq_ui(ui, gains, rate);
        return;
    }
    egui::Grid::new(egui::Id::unique(("fxgrid", id)))
        .num_columns(2)
        .spacing([12.0, 6.0])
        .show(ui, |ui| match fx {
            Effect::Amplify { db } => row(ui, "Amplification:", |ui| {
                ui.add(db_drag(db, -50.0, 50.0));
            }),
            Effect::Normalize { peak_db, remove_dc } => {
                row(ui, "Normalize peak to:", |ui| {
                    ui.add(db_drag(peak_db, -60.0, 0.0));
                });
                row(ui, "", |ui| {
                    ui.checkbox(remove_dc, "Remove DC offset");
                });
            }
            Effect::FadeIn | Effect::FadeOut | Effect::Reverse | Effect::Invert => {
                row(ui, "No settings.", |_| {})
            }
            Effect::Echo { delay_secs, decay } => {
                row(ui, "Delay time:", |ui| {
                    ui.add(
                        DragValue::new(delay_secs)
                            .speed(0.01)
                            .range(0.001..=10.0)
                            .suffix(" s"),
                    );
                });
                row(ui, "Decay factor:", |ui| {
                    ui.add(Slider::new(decay, 0.0..=0.95));
                });
            }
            Effect::LowPass { cutoff_hz } | Effect::HighPass { cutoff_hz } => {
                row(ui, "Cutoff:", |ui| {
                    ui.add(hz_drag(cutoff_hz, 10.0, nyq));
                })
            }
            Effect::NoiseReduction {
                reduction_db,
                sensitivity_db,
                smoothing,
            } => {
                row(ui, "Noise reduction:", |ui| {
                    ui.add(Slider::new(reduction_db, 0.0..=48.0).suffix(" dB"));
                });
                row(ui, "Sensitivity:", |ui| {
                    ui.add(Slider::new(sensitivity_db, 0.0..=24.0).suffix(" dB"))
                        .on_hover_text("Higher = treat more of the audio as noise");
                });
                row(ui, "Frequency smoothing:", |ui| {
                    ui.add(Slider::new(smoothing, 0..=12).suffix(" bands"));
                });
            }
            Effect::ClickRemoval {
                sensitivity,
                max_width_ms,
            } => {
                row(ui, "Sensitivity:", |ui| {
                    ui.add(Slider::new(sensitivity, 1.0..=10.0));
                });
                row(ui, "Max click width:", |ui| {
                    ui.add(ms_drag(max_width_ms, 0.1, 10.0));
                });
            }
            Effect::HumRemoval {
                base_hz,
                harmonics,
                q,
            } => {
                row(ui, "Mains frequency:", |ui| {
                    ui.radio_value(base_hz, 60.0, "60 Hz");
                    ui.radio_value(base_hz, 50.0, "50 Hz");
                });
                row(ui, "Harmonics:", |ui| {
                    ui.add(Slider::new(harmonics, 1..=20));
                });
                row(ui, "Notch narrowness (Q):", |ui| {
                    ui.add(Slider::new(q, 5.0..=100.0));
                });
            }
            Effect::DeEsser {
                threshold_db,
                freq_hz,
                max_reduction_db,
            } => {
                row(ui, "Threshold:", |ui| {
                    ui.add(Slider::new(threshold_db, -60.0..=0.0).suffix(" dB"));
                });
                row(ui, "Sibilance from:", |ui| {
                    ui.add(hz_drag(freq_hz, 2000.0, 12000.0f32.min(nyq)));
                });
                row(ui, "Max reduction:", |ui| {
                    ui.add(Slider::new(max_reduction_db, 0.0..=24.0).suffix(" dB"));
                });
            }
            Effect::ClipRepair {
                threshold,
                reduce_db,
            } => {
                row(ui, "Clipping threshold:", |ui| {
                    ui.add(
                        Slider::new(threshold, 0.5..=1.0)
                            .custom_formatter(|v, _| format!("{:.0}% of peak", v * 100.0)),
                    );
                });
                row(ui, "Reduce level by:", |ui| {
                    ui.add(Slider::new(reduce_db, 0.0..=12.0).suffix(" dB"));
                });
            }
            Effect::Compressor {
                threshold_db,
                ratio,
                attack_ms,
                release_ms,
                knee_db,
                makeup_db,
            } => {
                row(ui, "Threshold:", |ui| {
                    ui.add(Slider::new(threshold_db, -60.0..=0.0).suffix(" dB"));
                });
                row(ui, "Ratio:", |ui| {
                    ui.add(
                        Slider::new(ratio, 1.0..=20.0)
                            .logarithmic(true)
                            .custom_formatter(|v, _| format!("{v:.1}:1")),
                    );
                });
                row(ui, "Attack:", |ui| {
                    ui.add(ms_drag(attack_ms, 0.1, 500.0));
                });
                row(ui, "Release:", |ui| {
                    ui.add(ms_drag(release_ms, 1.0, 5000.0));
                });
                row(ui, "Knee:", |ui| {
                    ui.add(Slider::new(knee_db, 0.0..=24.0).suffix(" dB"));
                });
                row(ui, "Make-up gain:", |ui| {
                    ui.add(Slider::new(makeup_db, 0.0..=24.0).suffix(" dB"));
                });
            }
            Effect::Limiter {
                ceiling_db,
                release_ms,
            } => {
                row(ui, "Ceiling:", |ui| {
                    ui.add(Slider::new(ceiling_db, -12.0..=0.0).suffix(" dB"));
                });
                row(ui, "Release:", |ui| {
                    ui.add(ms_drag(release_ms, 1.0, 1000.0));
                });
            }
            Effect::NoiseGate {
                threshold_db,
                reduction_db,
                attack_ms,
                hold_ms,
                release_ms,
            } => {
                row(ui, "Threshold:", |ui| {
                    ui.add(Slider::new(threshold_db, -80.0..=0.0).suffix(" dB"));
                });
                row(ui, "Reduce gaps by:", |ui| {
                    ui.add(Slider::new(reduction_db, 0.0..=80.0).suffix(" dB"));
                });
                row(ui, "Attack:", |ui| {
                    ui.add(ms_drag(attack_ms, 0.1, 100.0));
                });
                row(ui, "Hold:", |ui| {
                    ui.add(ms_drag(hold_ms, 0.0, 1000.0));
                });
                row(ui, "Release:", |ui| {
                    ui.add(ms_drag(release_ms, 1.0, 2000.0));
                });
            }
            Effect::LoudnessNormalize {
                target_lufs,
                limit_peaks,
            } => {
                row(ui, "Target loudness:", |ui| {
                    ui.add(
                        DragValue::new(target_lufs)
                            .speed(0.1)
                            .range(-40.0..=-5.0)
                            .suffix(" LUFS"),
                    );
                });
                row(ui, "Presets:", |ui| {
                    for (label, v) in [
                        ("Streaming −14", -14.0),
                        ("Podcast −16", -16.0),
                        ("Broadcast −23", -23.0),
                    ] {
                        if ui.small_button(label).clicked() {
                            *target_lufs = v;
                        }
                    }
                });
                row(ui, "", |ui| {
                    ui.checkbox(limit_peaks, "Limit peaks to −1 dB");
                });
            }
            Effect::GraphicEq { .. } => {}
            Effect::BassTreble { bass_db, treble_db } => {
                row(ui, "Bass:", |ui| {
                    ui.add(Slider::new(bass_db, -24.0..=24.0).suffix(" dB"));
                });
                row(ui, "Treble:", |ui| {
                    ui.add(Slider::new(treble_db, -24.0..=24.0).suffix(" dB"));
                });
            }
            Effect::Distortion {
                kind,
                drive_db,
                mix,
                output_db,
            } => {
                row(ui, "Type:", |ui| {
                    egui::ComboBox::from_id_salt(("dist", ui.next_auto_id()))
                        .selected_text(format!("{kind:?}"))
                        .show_ui(ui, |ui| {
                            for k in DistortionKind::ALL {
                                ui.selectable_value(kind, k, format!("{k:?}"));
                            }
                        });
                });
                row(ui, "Drive:", |ui| {
                    ui.add(Slider::new(drive_db, 0.0..=48.0).suffix(" dB"));
                });
                row(ui, "Mix (wet):", |ui| {
                    ui.add(Slider::new(mix, 0.0..=1.0));
                });
                row(ui, "Output:", |ui| {
                    ui.add(Slider::new(output_db, -24.0..=6.0).suffix(" dB"));
                });
            }
            Effect::ChangeSpeed { factor } => {
                let mut pct = (*factor - 1.0) * 100.0;
                row(ui, "Speed change:", |ui| {
                    if ui
                        .add(
                            DragValue::new(&mut pct)
                                .speed(0.5)
                                .range(-90.0..=400.0)
                                .suffix(" %"),
                        )
                        .changed()
                    {
                        *factor = 1.0 + pct / 100.0;
                    }
                });
            }
            Effect::ChangeTempo { percent } => row(ui, "Tempo change:", |ui| {
                ui.add(
                    DragValue::new(percent)
                        .speed(0.5)
                        .range(-90.0..=400.0)
                        .suffix(" %"),
                );
            }),
            Effect::ChangePitch { semitones } => {
                row(ui, "Pitch change:", |ui| {
                    ui.add(
                        DragValue::new(semitones)
                            .speed(0.05)
                            .range(-24.0..=24.0)
                            .suffix(" semitones")
                            .max_decimals(2),
                    );
                });
                row(ui, "", |ui| {
                    ui.label(format!(
                        "{:+.1} cents total, ×{:.3} frequency",
                        *semitones * 100.0,
                        2f32.powf(*semitones / 12.0)
                    ));
                });
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
                row(ui, "Room size:", |ui| {
                    ui.add(Slider::new(room, 0.0..=1.0));
                });
                row(ui, "Damping:", |ui| {
                    ui.add(Slider::new(damping, 0.0..=1.0));
                });
                row(ui, "Wet level:", |ui| {
                    ui.add(Slider::new(wet_db, -40.0..=6.0).suffix(" dB"));
                });
                row(ui, "Dry level:", |ui| {
                    ui.add(Slider::new(dry_db, -60.0..=6.0).suffix(" dB"));
                });
                row(ui, "Stereo width:", |ui| {
                    ui.add(Slider::new(width, 0.0..=1.0));
                });
                row(ui, "Pre-delay:", |ui| {
                    ui.add(ms_drag(pre_delay_ms, 0.0, 200.0));
                });
                row(ui, "Tail length:", |ui| {
                    ui.add(
                        DragValue::new(tail_secs)
                            .speed(0.05)
                            .range(0.0..=10.0)
                            .suffix(" s"),
                    );
                });
            }
            Effect::StereoWidener {
                width,
                mid_db,
                side_db,
            } => {
                row(ui, "Width:", |ui| {
                    ui.add(Slider::new(width, 0.0..=2.5));
                });
                row(ui, "Mid (centre) level:", |ui| {
                    ui.add(Slider::new(mid_db, -24.0..=12.0).suffix(" dB"));
                });
                row(ui, "Side level:", |ui| {
                    ui.add(Slider::new(side_db, -24.0..=12.0).suffix(" dB"));
                });
            }
            Effect::VocalRemover { keep_bass_hz } => row(ui, "Keep bass below:", |ui| {
                ui.add(hz_drag(keep_bass_hz, 0.0, 500.0))
                    .on_hover_text("0 = cancel everything in the centre, including bass and kick");
            }),
        });
}

fn graphic_eq_ui(ui: &mut Ui, gains: &mut [f32; 10], rate: u32) {
    ui.horizontal(|ui| {
        ui.label("Preset:");
        egui::ComboBox::from_id_salt("eq_preset")
            .selected_text("Choose…")
            .show_ui(ui, |ui| {
                for (name, g) in EQ_PRESETS {
                    if ui.selectable_label(false, *name).clicked() {
                        *gains = *g;
                    }
                }
            });
        if ui.button("Flat").clicked() {
            *gains = [0.0; 10];
        }
    });
    // Response curve.
    let (rect, _) = ui.allocate_exact_size(vec2(440.0, 110.0), Sense::hover());
    let p = ui.painter_at(rect);
    p.rect_filled(rect, CornerRadius::same(3), Color32::from_gray(25));
    let range = 18.0;
    let y_of = |db: f32| rect.center().y - (db / range).clamp(-1.0, 1.0) * rect.height() * 0.45;
    for db in [-12.0, -6.0, 0.0, 6.0, 12.0] {
        let y = y_of(db);
        p.line_segment(
            [pos2(rect.left(), y), pos2(rect.right(), y)],
            Stroke::new(1.0, Color32::from_gray(if db == 0.0 { 70 } else { 45 })),
        );
    }
    let (fmin, fmax) = (20f32, (rate as f32 / 2.0).min(20000.0));
    let x_of = |f: f32| rect.left() + rect.width() * ((f / fmin).ln() / (fmax / fmin).ln());
    let pts: Vec<Pos2> = (0..=200)
        .map(|i| {
            let f = fmin * (fmax / fmin).powf(i as f32 / 200.0);
            pos2(x_of(f), y_of(tone::eq_response(gains, rate as f32, f)))
        })
        .collect();
    p.add(egui::Shape::line(
        pts,
        Stroke::new(2.0, Color32::from_rgb(120, 200, 255)),
    ));
    // Sliders.
    ui.horizontal(|ui| {
        for (g, f) in gains.iter_mut().zip(EQ_BANDS) {
            ui.vertical(|ui| {
                ui.add(
                    Slider::new(g, -18.0..=18.0)
                        .vertical()
                        .show_value(false)
                        .step_by(0.5),
                );
                ui.label(egui::RichText::new(format!("{g:+.1}")).small());
                let label = if f >= 1000.0 {
                    format!("{}k", f / 1000.0)
                } else {
                    format!("{f}")
                };
                ui.label(egui::RichText::new(label).small().strong());
            });
        }
    });
}
