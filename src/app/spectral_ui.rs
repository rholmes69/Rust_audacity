//! Spectral selection (drag a box on a track in Spectrum view) and Effect ▸ Spectral Edit.

use eframe::egui::{self, Color32, CornerRadius, Painter, Rect, Stroke, StrokeKind, Ui, pos2};

use super::AudacityApp;
use crate::dsp::{specgram, spectral::SpectralOp};

pub struct SpectralState {
    /// Selected frequency range (Hz), together with the time selection.
    pub sel: Option<(f32, f32)>,
    /// Frequency where the current drag started.
    pub anchor: Option<f32>,
    pub reduce_db: f32,
}

impl Default for SpectralState {
    fn default() -> Self {
        Self {
            sel: None,
            anchor: None,
            reduce_db: 12.0,
        }
    }
}

fn fmt_hz(f: f32) -> String {
    if f >= 1000.0 {
        format!("{:.2} kHz", f / 1000.0)
    } else {
        format!("{f:.0} Hz")
    }
}

impl AudacityApp {
    /// Frequency under `y` in a spectrogram track drawn in `rect`.
    pub(super) fn freq_at_y(&self, i: usize, rect: Rect, y: f32) -> f32 {
        let nch = self.project.tracks[i].num_channels().max(1);
        let lane_h = rect.height() / nch as f32;
        let k = (((y - rect.top()) / lane_h).floor().max(0.0) as usize).min(nch - 1);
        let top = rect.top() + k as f32 * lane_h + 1.0;
        let frac = ((y - top) / (lane_h - 2.0).max(1.0)).clamp(0.0, 1.0);
        specgram::freq_at(frac, self.project.rate)
    }

    /// Box outline on each lane of a spectrogram track.
    pub(super) fn draw_spectral_box(&self, painter: &Painter, i: usize, rect: Rect) {
        let Some((lo, hi)) = self.spectral.sel else {
            return;
        };
        let t = &self.project.tracks[i];
        let sel = self.project.selection;
        if !t.selected || sel.is_empty() || t.view != crate::track::TrackView::Spectrogram {
            return;
        }
        let x0 = self
            .view
            .sample_to_x(rect, sel.start as f64)
            .max(rect.left());
        let x1 = self
            .view
            .sample_to_x(rect, sel.end as f64)
            .min(rect.right());
        if x1 <= x0 {
            return;
        }
        let nch = t.num_channels().max(1);
        let lane_h = rect.height() / nch as f32;
        for k in 0..nch {
            let top = rect.top() + k as f32 * lane_h + 1.0;
            let h = lane_h - 2.0;
            let y_hi = top + specgram::frac_of(hi, self.project.rate) * h;
            let y_lo = top + specgram::frac_of(lo, self.project.rate) * h;
            let b = Rect::from_min_max(pos2(x0, y_hi.min(y_lo)), pos2(x1, y_lo.max(y_hi)));
            painter.rect_filled(
                b,
                CornerRadius::ZERO,
                Color32::from_rgba_unmultiplied(255, 255, 255, 28),
            );
            painter.rect_stroke(
                b,
                CornerRadius::ZERO,
                Stroke::new(1.5, Color32::WHITE),
                StrokeKind::Inside,
            );
        }
    }

    pub(super) fn spectral_label(&self) -> Option<String> {
        let (lo, hi) = self.spectral.sel?;
        (!self.project.selection.is_empty()).then(|| format!("{} – {}", fmt_hz(lo), fmt_hz(hi)))
    }

    pub(super) fn do_spectral(&mut self, op: SpectralOp) {
        let Some((lo, hi)) = self.spectral.sel else {
            self.status = "Drag a box on a track in Spectrum view first".into();
            return;
        };
        if self.busy() {
            return;
        }
        self.engine.stop_playback();
        if self.project.spectral_edit(lo, hi, op) {
            self.status = format!("{} ({} – {})", op.name(), fmt_hz(lo), fmt_hz(hi));
        } else {
            self.status = "Select some time as well as a frequency range".into();
        }
    }

    /// Effect ▸ Spectral Edit.
    pub(super) fn spectral_menu(&mut self, ui: &mut Ui) {
        ui.menu_button("Spectral Edit", |ui| {
            let ready =
                self.spectral.sel.is_some() && !self.project.selection.is_empty() && !self.busy();
            match self.spectral_label() {
                Some(l) => {
                    ui.label(egui::RichText::new(format!("Selected: {l}")).small().weak());
                }
                None => {
                    ui.label(
                        egui::RichText::new(
                            "Switch a track to Spectrum view and drag a box over the sound.",
                        )
                        .small()
                        .weak(),
                    );
                }
            }
            ui.separator();
            if ui
                .add_enabled(ready, egui::Button::new("Delete Frequencies"))
                .on_hover_text("Remove the boxed frequencies (e.g. a whistle or a beep)")
                .clicked()
            {
                self.do_spectral(SpectralOp::Delete);
                ui.close();
            }
            ui.horizontal(|ui| {
                if ui
                    .add_enabled(ready, egui::Button::new("Reduce by"))
                    .clicked()
                {
                    let db = self.spectral.reduce_db;
                    self.do_spectral(SpectralOp::Reduce(db));
                    ui.close();
                }
                ui.add(
                    egui::DragValue::new(&mut self.spectral.reduce_db)
                        .range(1.0..=60.0)
                        .suffix(" dB"),
                );
            });
            if ui
                .add_enabled(ready, egui::Button::new("Heal"))
                .on_hover_text(
                    "Fill the box with what's around it in time (coughs, clicks, chair squeaks)",
                )
                .clicked()
            {
                self.do_spectral(SpectralOp::Heal);
                ui.close();
            }
            if ui
                .add_enabled(ready, egui::Button::new("Isolate Frequencies"))
                .on_hover_text("Keep only the boxed frequencies during the selection")
                .clicked()
            {
                self.do_spectral(SpectralOp::Isolate);
                ui.close();
            }
            ui.separator();
            if ui
                .add_enabled(
                    self.spectral.sel.is_some(),
                    egui::Button::new("Clear Frequency Selection"),
                )
                .clicked()
            {
                self.spectral.sel = None;
                ui.close();
            }
        });
    }
}
