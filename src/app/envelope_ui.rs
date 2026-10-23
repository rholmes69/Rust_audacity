//! Volume envelopes: drawing the curve, the Envelope tool (F2), and the Auto Duck dialog.

use eframe::egui::{self, Align2, Color32, Pos2, Rect, RichText, Stroke, Ui, pos2, vec2};

use super::{AudacityApp, Dialog, Tool};
use crate::{
    dsp::duck::DuckParams,
    envelope::{MAX_DB, MIN_DB},
};

/// dB shown at the top / bottom of a track while drawing envelopes.
const TOP_DB: f32 = MAX_DB;
const BOTTOM_DB: f32 = MIN_DB;
const HIT_PX: f32 = 7.0;
const ENV_COLOR: Color32 = Color32::from_rgb(255, 170, 60);

fn db_to_y(r: Rect, db: f32) -> f32 {
    let f = (TOP_DB - db.clamp(BOTTOM_DB, TOP_DB)) / (TOP_DB - BOTTOM_DB);
    r.top() + 4.0 + f * (r.height() - 8.0)
}

fn y_to_db(r: Rect, y: f32) -> f32 {
    let f = ((y - r.top() - 4.0) / (r.height() - 8.0)).clamp(0.0, 1.0);
    let db = TOP_DB - f * (TOP_DB - BOTTOM_DB);
    // Snap near 0 dB so it's easy to get back to "unchanged".
    if db.abs() < 0.75 {
        0.0
    } else {
        (db * 2.0).round() / 2.0
    }
}

/// An in-progress drag of an envelope point.
pub(super) struct EnvDrag {
    track: usize,
    point: usize,
}

pub struct DuckState {
    pub open: bool,
    voice: usize,
    targets: Vec<bool>,
    params: DuckParams,
    use_words: bool,
}

impl Default for DuckState {
    fn default() -> Self {
        Self {
            open: false,
            voice: 0,
            targets: Vec::new(),
            params: DuckParams::default(),
            use_words: true,
        }
    }
}

impl AudacityApp {
    /// Draw a track's envelope (always when it has points; as a flat line in Envelope mode).
    pub(super) fn draw_envelope(&self, painter: &egui::Painter, i: usize, rect: Rect, left: f64) {
        let env = &self.project.tracks[i].envelope;
        let editing = self.tool == Tool::Envelope;
        if env.is_empty() && !editing {
            return;
        }
        if editing {
            // 0 dB reference and a light scale.
            for (db, label) in [(0.0, "0 dB"), (-12.0, "−12"), (-24.0, "−24")] {
                let y = db_to_y(rect, db);
                painter.line_segment(
                    [pos2(rect.left(), y), pos2(rect.right(), y)],
                    Stroke::new(
                        1.0,
                        Color32::from_white_alpha(if db == 0.0 { 40 } else { 18 }),
                    ),
                );
                painter.text(
                    pos2(rect.right() - 4.0, y - 1.0),
                    Align2::RIGHT_BOTTOM,
                    label,
                    egui::FontId::proportional(10.0),
                    Color32::from_white_alpha(110),
                );
            }
        }
        let spp = self.view.spp;
        let step = 3.0;
        let mut line: Vec<Pos2> = Vec::new();
        let mut x = rect.left();
        while x <= rect.right() {
            let s = left + (x - rect.left()) as f64 * spp;
            line.push(pos2(x, db_to_y(rect, env.db_at(s))));
            x += step;
        }
        let alpha = if editing { 255 } else { 170 };
        let col =
            Color32::from_rgba_unmultiplied(ENV_COLOR.r(), ENV_COLOR.g(), ENV_COLOR.b(), alpha);
        painter.add(egui::Shape::line(
            line,
            Stroke::new(if editing { 2.0 } else { 1.5 }, col),
        ));
        for p in &env.points {
            let x = rect.left() + ((p.pos as f64 - left) / spp) as f32;
            if x < rect.left() - 5.0 || x > rect.right() + 5.0 {
                continue;
            }
            let c = pos2(x, db_to_y(rect, p.db));
            if editing {
                painter.circle(c, 4.5, Color32::from_rgb(40, 30, 10), Stroke::new(2.0, col));
            } else {
                painter.circle_filled(c, 2.5, col);
            }
        }
    }

    fn hit_point(&self, i: usize, rect: Rect, ptr: Pos2) -> Option<usize> {
        let env = &self.project.tracks[i].envelope;
        env.points
            .iter()
            .enumerate()
            .map(|(k, p)| {
                let x = self.view.sample_to_x(rect, p.pos as f64);
                (k, pos2(x, db_to_y(rect, p.db)).distance(ptr))
            })
            .filter(|(_, d)| *d <= HIT_PX)
            .min_by(|a, b| a.1.total_cmp(&b.1))
            .map(|(k, _)| k)
    }

    /// Envelope tool: click to add a point, drag to move, right-click (or Alt+click) to delete.
    pub(super) fn envelope_interaction(
        &mut self,
        ui: &Ui,
        i: usize,
        rect: Rect,
        resp: &egui::Response,
    ) {
        let hover = ui.input(|inp| inp.pointer.hover_pos());
        let alt = ui.input(|inp| inp.modifiers.alt);
        let sample_at =
            |app: &Self, x: f32| app.view.x_to_sample(rect, x).max(0.0).round() as usize;

        if resp.drag_started()
            && let Some(p) = ui.input(|inp| inp.pointer.press_origin())
        {
            let hit = self.hit_point(i, rect, p);
            self.project.push_undo("Envelope");
            let point = match hit {
                Some(k) => k,
                None => {
                    let s = sample_at(self, p.x);
                    self.project.tracks[i].envelope.add(s, y_to_db(rect, p.y))
                }
            };
            self.env_drag = Some(EnvDrag { track: i, point });
        }
        if resp.dragged()
            && let Some(p) = ui.input(|inp| inp.pointer.interact_pos())
            && let Some(d) = &self.env_drag
            && d.track == i
        {
            let s = sample_at(self, p.x);
            let db = y_to_db(rect, p.y);
            let k = d.point;
            let nk = self.project.tracks[i].envelope.set(k, s, db);
            if let Some(d) = &mut self.env_drag {
                d.point = nk;
            }
            self.project.touch();
            self.status = format!("Envelope point: {db:+.1} dB at {}", self.fmt_time(s as f64));
        }
        if resp.drag_stopped() {
            self.env_drag = None;
        }
        if (resp.clicked() || resp.secondary_clicked())
            && let Some(p) = resp.interact_pointer_pos()
        {
            match self.hit_point(i, rect, p) {
                Some(k) if resp.secondary_clicked() || alt => {
                    self.project.push_undo("Delete Envelope Point");
                    self.project.tracks[i].envelope.points.remove(k);
                    self.status = "Envelope point removed".into();
                }
                Some(_) => {}
                None if resp.clicked() && !alt => {
                    self.project.push_undo("Envelope");
                    let s = sample_at(self, p.x);
                    let db = y_to_db(rect, p.y);
                    self.project.tracks[i].envelope.add(s, db);
                    self.status = format!("Added envelope point: {db:+.1} dB");
                }
                None => {}
            }
        }
        if let Some(h) = hover
            && rect.contains(h)
        {
            let on_point = self.hit_point(i, rect, h).is_some();
            ui.ctx().set_cursor_icon(if resp.dragged() {
                egui::CursorIcon::Grabbing
            } else if on_point {
                egui::CursorIcon::Grab
            } else {
                egui::CursorIcon::Crosshair
            });
            if !resp.dragged() {
                let env = &self.project.tracks[i].envelope;
                let s = self.view.x_to_sample(rect, h.x).max(0.0);
                let hint = format!("{:+.1} dB", if env.is_empty() { 0.0 } else { env.db_at(s) });
                ui.painter().text(
                    h + vec2(12.0, -12.0),
                    Align2::LEFT_BOTTOM,
                    hint,
                    egui::FontId::proportional(11.0),
                    ENV_COLOR,
                );
            }
        }
    }

    pub(super) fn clear_envelopes(&mut self) {
        let targets: Vec<usize> = self
            .project
            .target_tracks()
            .into_iter()
            .filter(|&i| !self.project.tracks[i].envelope.is_empty())
            .collect();
        if targets.is_empty() {
            self.status = "The selected tracks have no envelope".into();
            return;
        }
        self.project.push_undo("Clear Envelope");
        for i in targets {
            self.project.tracks[i].envelope.points.clear();
        }
        self.status = "Envelope cleared".into();
    }

    // ------------------------------------------------------------ auto duck

    pub(super) fn open_duck_dialog(&mut self) {
        let n = self.project.tracks.len();
        if n < 2 {
            self.dialog = Some(Dialog::Message(
                "Auto Duck needs at least two tracks: a voice track and the music to lower under it.".into(),
            ));
            return;
        }
        // The transcribed track, else the track with the most pauses (speech).
        let voice = self.project.transcript_track().unwrap_or_else(|| {
            let rate = self.project.rate;
            (0..n)
                .map(|i| {
                    (
                        i,
                        crate::dsp::duck::pause_fraction(&self.project.tracks[i].channels[0], rate),
                    )
                })
                .max_by(|a, b| a.1.total_cmp(&b.1))
                .map_or(0, |(i, _)| i)
        });
        self.duck.voice = voice;
        self.duck.targets = (0..n).map(|i| i != voice).collect();
        self.duck.open = true;
    }

    pub(super) fn run_auto_duck(
        &mut self,
        voice: usize,
        targets: &[usize],
        params: &DuckParams,
        use_words: bool,
    ) -> Result<usize, String> {
        if self.busy() {
            return Err("Rust Audacity is busy.".into());
        }
        self.engine.stop_playback();
        let n = self.project.auto_duck(voice, targets, params, use_words)?;
        self.status = format!(
            "Auto Duck: {} lowered by {:.0} dB during {n} stretch(es) of speech. Fine-tune with the Envelope tool (F2).",
            if targets.len() == 1 {
                format!("“{}”", self.project.tracks[targets[0]].name)
            } else {
                format!("{} tracks", targets.len())
            },
            -params.amount_db
        );
        Ok(n)
    }

    pub(super) fn duck_window(&mut self, ctx: &egui::Context) {
        if !self.duck.open {
            return;
        }
        let n = self.project.tracks.len();
        if n < 2 {
            self.duck.open = false;
            return;
        }
        self.duck.targets.resize(n, false);
        self.duck.voice = self.duck.voice.min(n - 1);
        let mut open = true;
        let (mut go, mut cancel) = (false, false);
        let has_words = self.project.transcript_track() == Some(self.duck.voice);
        egui::Window::new("Auto Duck")
            .collapsible(false)
            .resizable(false)
            .anchor(Align2::CENTER_CENTER, vec2(0.0, 0.0))
            .open(&mut open)
            .show(ctx, |ui| {
                ui.label(
                    RichText::new(
                        "Lowers music whenever someone speaks. The fades are written as volume-envelope \
                         points you can adjust afterwards with the Envelope tool (F2).",
                    )
                    .weak(),
                );
                ui.add_space(6.0);
                let d = &mut self.duck;
                egui::Grid::new("duck").num_columns(2).spacing([12.0, 6.0]).show(ui, |ui| {
                    ui.label("Voice track:");
                    egui::ComboBox::from_id_salt("duck_voice")
                        .selected_text(self.project.tracks[d.voice].name.clone())
                        .show_ui(ui, |ui| {
                            for (k, t) in self.project.tracks.iter().enumerate() {
                                if ui.selectable_label(d.voice == k, &t.name).clicked() {
                                    d.voice = k;
                                    d.targets = (0..n).map(|i| i != k).collect();
                                }
                            }
                        });
                    ui.end_row();
                    ui.label("Lower these:");
                    ui.vertical(|ui| {
                        for (k, t) in self.project.tracks.iter().enumerate() {
                            if k != d.voice {
                                ui.checkbox(&mut d.targets[k], &t.name);
                            }
                        }
                    });
                    ui.end_row();
                    ui.label("Lower by:");
                    ui.add(egui::Slider::new(&mut d.params.amount_db, -40.0..=-1.0).suffix(" dB"));
                    ui.end_row();
                    ui.label("Speech detection:");
                    if has_words {
                        ui.checkbox(&mut d.use_words, "Use the transcript's words (most accurate)");
                    } else {
                        ui.label(RichText::new("by level").weak());
                    }
                    ui.end_row();
                    if !(has_words && d.use_words) {
                        ui.label("Speech threshold:");
                        ui.add(egui::Slider::new(&mut d.params.threshold_db, -60.0..=-10.0).suffix(" dB"))
                            .on_hover_text("Lower = more sensitive. Raise it if background noise on the voice track triggers ducking.");
                        ui.end_row();
                    }
                    ui.label("Fade down:");
                    ui.add(egui::Slider::new(&mut d.params.fade_down_ms, 50.0..=2000.0).suffix(" ms"));
                    ui.end_row();
                    ui.label("Fade up:");
                    ui.add(egui::Slider::new(&mut d.params.fade_up_ms, 100.0..=4000.0).suffix(" ms"));
                    ui.end_row();
                    ui.label("Start early by:");
                    ui.add(egui::Slider::new(&mut d.params.lead_ms, 0.0..=1000.0).suffix(" ms"));
                    ui.end_row();
                    ui.label("Stay down through pauses up to:");
                    ui.add(egui::Slider::new(&mut d.params.hold_ms, 100.0..=5000.0).suffix(" ms"));
                    ui.end_row();
                });
                ui.label(
                    RichText::new("This replaces any envelope already on the lowered tracks (you can undo).")
                        .small()
                        .weak(),
                );
                ui.separator();
                ui.horizontal(|ui| {
                    if ui.button("  Duck  ").clicked() {
                        go = true;
                    }
                    if ui.button("Reset").clicked() {
                        self.duck.params = DuckParams::default();
                    }
                    if ui.button("Cancel").clicked() {
                        cancel = true;
                    }
                });
            });
        if !open || cancel {
            self.duck.open = false;
        }
        if go {
            let targets: Vec<usize> = (0..n)
                .filter(|&k| self.duck.targets[k] && k != self.duck.voice)
                .collect();
            let (voice, params, words) = (
                self.duck.voice,
                self.duck.params,
                self.duck.use_words && has_words,
            );
            match self.run_auto_duck(voice, &targets, &params, words) {
                Ok(_) => self.duck.open = false,
                Err(e) => self.dialog = Some(Dialog::Message(e)),
            }
        }
    }
}
