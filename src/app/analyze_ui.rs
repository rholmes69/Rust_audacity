//! Analyze ▸ Plot Spectrum & Loudness Stats window.

use eframe::egui::{
    self, Align2, Color32, CornerRadius, FontId, RichText, Sense, Stroke, pos2, vec2,
};

use super::AudacityApp;
use crate::dsp::analysis::{self, Stats};

const FFT_N: usize = 8192;
const DB_MIN: f32 = -110.0;

pub struct AnalysisWindow {
    spectrum: Vec<f32>,
    rate: u32,
    stats: Stats,
    range: String,
}

fn note_name(freq: f32) -> String {
    if freq < 16.0 {
        return String::new();
    }
    const NAMES: [&str; 12] = [
        "C", "C#", "D", "D#", "E", "F", "F#", "G", "G#", "A", "A#", "B",
    ];
    let midi = (69.0 + 12.0 * (freq / 440.0).log2()).round() as i32;
    format!("{}{}", NAMES[midi.rem_euclid(12) as usize], midi / 12 - 1)
}

fn fmt_db(v: f32, unit: &str) -> String {
    if v.is_finite() {
        format!("{v:.1} {unit}")
    } else {
        "−∞".into()
    }
}

impl AudacityApp {
    pub(super) fn open_analysis(&mut self) {
        let (s, e) = self.project.effect_range();
        if self.project.tracks.is_empty() || e <= s {
            self.status = "Nothing to analyze".into();
            return;
        }
        let mix = self.project.mixdown(s, e, true);
        let mono: Vec<f32> = if mix.len() == 1 {
            mix[0].clone()
        } else {
            mix[0]
                .iter()
                .zip(&mix[1])
                .map(|(a, b)| (a + b) * 0.5)
                .collect()
        };
        let range = if self.project.selection.is_empty() {
            "whole project".to_string()
        } else {
            format!("{} – {}", self.fmt_time(s as f64), self.fmt_time(e as f64))
        };
        self.analysis = Some(AnalysisWindow {
            spectrum: analysis::spectrum(&mono, FFT_N),
            rate: self.project.rate,
            stats: analysis::measure(&mix, self.project.rate),
            range,
        });
    }

    pub(super) fn analysis_window(&mut self, ctx: &egui::Context) {
        let Some(a) = &self.analysis else { return };
        let mut open = true;
        let mut refresh = false;
        egui::Window::new("Frequency Analysis & Loudness").open(&mut open).resizable(false).show(ctx, |ui| {
            ui.label(RichText::new(format!("Selected tracks, {}", a.range)).weak());
            let (rect, resp) = ui.allocate_exact_size(vec2(640.0, 300.0), Sense::hover());
            let p = ui.painter_at(rect);
            p.rect_filled(rect, CornerRadius::same(3), Color32::from_gray(22));
            let plot = rect.shrink2(vec2(36.0, 16.0)).translate(vec2(14.0, -4.0));
            let fmin = 20f32;
            let fmax = a.rate as f32 / 2.0;
            let x_of = |f: f32| plot.left() + plot.width() * ((f.max(fmin) / fmin).ln() / (fmax / fmin).ln());
            let f_of = |x: f32| fmin * (fmax / fmin).powf(((x - plot.left()) / plot.width()).clamp(0.0, 1.0));
            let y_of = |db: f32| plot.top() + plot.height() * (db.clamp(DB_MIN, 0.0) / DB_MIN);
            let grid = Stroke::new(1.0, Color32::from_gray(48));
            let text = Color32::from_gray(160);
            for db in (0..=100).step_by(20) {
                let y = y_of(-(db as f32));
                p.line_segment([pos2(plot.left(), y), pos2(plot.right(), y)], grid);
                p.text(pos2(plot.left() - 4.0, y), Align2::RIGHT_CENTER, format!("-{db}"), FontId::proportional(10.0), text);
            }
            for f in [20.0, 50.0, 100.0, 200.0, 500.0, 1000.0, 2000.0, 5000.0, 10000.0, 20000.0] {
                if f > fmax {
                    continue;
                }
                let x = x_of(f);
                p.line_segment([pos2(x, plot.top()), pos2(x, plot.bottom())], grid);
                let label = if f >= 1000.0 { format!("{}k", f / 1000.0) } else { format!("{f}") };
                p.text(pos2(x, plot.bottom() + 2.0), Align2::CENTER_TOP, label, FontId::proportional(10.0), text);
            }
            // Spectrum: draw the max per pixel column so peaks are not lost.
            let bin_hz = a.rate as f32 / FFT_N as f32;
            let mut pts = Vec::new();
            let w = plot.width() as usize;
            for px in 0..w {
                let (f0, f1) = (f_of(plot.left() + px as f32), f_of(plot.left() + px as f32 + 1.0));
                let (b0, b1) = ((f0 / bin_hz) as usize, ((f1 / bin_hz).ceil() as usize).max((f0 / bin_hz) as usize + 1));
                let v = a.spectrum[b0.min(a.spectrum.len() - 1)..b1.min(a.spectrum.len())].iter().copied().fold(f32::MIN, f32::max);
                if v > f32::MIN {
                    pts.push(pos2(plot.left() + px as f32, y_of(v)));
                }
            }
            for wnd in pts.windows(2) {
                p.add(egui::Shape::convex_polygon(
                    vec![wnd[0], wnd[1], pos2(wnd[1].x, plot.bottom()), pos2(wnd[0].x, plot.bottom())],
                    Color32::from_rgba_unmultiplied(80, 140, 230, 60),
                    Stroke::NONE,
                ));
            }
            p.add(egui::Shape::line(pts, Stroke::new(1.5, Color32::from_rgb(120, 180, 255))));
            // Hover readout.
            if let Some(pos) = resp.hover_pos().filter(|p| plot.contains(*p)) {
                let f = f_of(pos.x);
                let bin = ((f / bin_hz).round() as usize).min(a.spectrum.len() - 1);
                // Snap to the local peak within ±3 bins.
                let lo = bin.saturating_sub(3);
                let hi = (bin + 3).min(a.spectrum.len() - 1);
                let (pb, pv) = (lo..=hi).map(|b| (b, a.spectrum[b])).fold((bin, f32::MIN), |m, x| if x.1 > m.1 { x } else { m });
                let pf = pb as f32 * bin_hz;
                p.line_segment([pos2(pos.x, plot.top()), pos2(pos.x, plot.bottom())], Stroke::new(1.0, Color32::from_white_alpha(80)));
                p.text(
                    pos2(plot.right() - 4.0, plot.top() + 4.0),
                    Align2::RIGHT_TOP,
                    format!("{pf:.0} Hz  {}   {pv:.1} dB", note_name(pf)),
                    FontId::monospace(12.0),
                    Color32::WHITE,
                );
            }

            ui.add_space(6.0);
            let st = &a.stats;
            egui::Grid::new("stats").num_columns(4).spacing([24.0, 4.0]).show(ui, |ui| {
                ui.label("Integrated loudness:");
                ui.strong(st.lufs.map_or("too short/quiet".into(), |v| format!("{v:.1} LUFS")));
                ui.label("Loudness range:");
                ui.label(st.lra.map_or("–".into(), |v| format!("{v:.1} LU")));
                ui.end_row();
                ui.label("Sample peak:");
                ui.label(fmt_db(st.peak_db, "dBFS"));
                ui.label("True peak:");
                ui.label(fmt_db(st.true_peak_db, "dBTP"));
                ui.end_row();
                ui.label("RMS level:");
                ui.label(fmt_db(st.rms_db, "dBFS"));
                ui.label("Crest factor:");
                ui.label(if st.crest_db.is_finite() { format!("{:.1} dB", st.crest_db) } else { "–".into() });
                ui.end_row();
                ui.label("DC offset:");
                ui.label(format!("{:+.4}", st.dc_offset));
                ui.label("Duration:");
                ui.label(format!("{:.2} s", st.duration_secs));
                ui.end_row();
            });
            if let Some(l) = st.lufs {
                ui.label(RichText::new(format!("To reach −14 LUFS (streaming): {:+.1} dB   ·   −16 LUFS (podcast): {:+.1} dB", -14.0 - l, -16.0 - l)).weak());
            }
            if ui.button("Refresh for current selection").clicked() {
                refresh = true;
            }
        });
        if refresh {
            self.open_analysis();
        }
        if !open {
            self.analysis = None;
        }
    }
}
