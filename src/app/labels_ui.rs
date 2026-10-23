//! Labels: the strip under the ruler (click to select, drag to move, drag edges to resize,
//! double-click to rename, right-click for a menu), the Labels window, Audacity label-file
//! import/export, and "Export audio per label".

use eframe::egui::{
    self, Align2, Color32, CornerRadius, FontId, Rect, RichText, Sense, Stroke, StrokeKind, Ui,
    pos2, vec2,
};

use super::{AudacityApp, Dialog};
use crate::{fileio::ExportFormat, labels, project::Selection};

pub const LABEL_H: f32 = 24.0;
const COL: Color32 = Color32::from_rgb(250, 205, 90);
const EDGE_PX: f32 = 5.0;

#[derive(Clone, Copy, PartialEq)]
enum DragMode {
    Move,
    Start,
    End,
}

struct LabelDrag {
    id: u64,
    mode: DragMode,
    /// Pointer sample minus label start when the drag began.
    grab: i64,
}

pub struct LabelsState {
    pub window_open: bool,
    /// Label being renamed: (id, text being edited, first frame?).
    renaming: Option<(u64, String, bool)>,
    drag: Option<LabelDrag>,
    pub selected: Option<u64>,
    menu_for: Option<u64>,
    export_fmt: ExportFormat,
    before_first: bool,
    numbered: bool,
}

impl Default for LabelsState {
    fn default() -> Self {
        Self {
            window_open: false,
            renaming: None,
            drag: None,
            selected: None,
            menu_for: None,
            export_fmt: ExportFormat::Mp3 { kbps: 192 },
            before_first: false,
            numbered: true,
        }
    }
}

impl AudacityApp {
    /// Ctrl+B: add a label at the selection (or cursor) and start renaming it.
    pub(super) fn add_label_at_selection(&mut self) {
        let sel = self.project.selection;
        let n = self.project.labels.len() + 1;
        let id = self
            .project
            .add_label(sel.start, sel.end, format!("Label {n}"));
        self.labels_ui.selected = Some(id);
        self.labels_ui.renaming = Some((id, format!("Label {n}"), true));
        self.status = "Label added: type a name and press Enter".into();
    }

    fn label_rect(&self, strip: Rect, l: &labels::Label, ui: &Ui) -> (f32, f32, Rect) {
        let x0 = self.view.sample_to_x(strip, l.start as f64);
        let x1 = self.view.sample_to_x(strip, l.end as f64);
        let galley =
            ui.painter()
                .layout_no_wrap(l.text.clone(), FontId::proportional(12.0), Color32::WHITE);
        let w = galley.size().x + 10.0;
        let chip = Rect::from_min_size(
            pos2(x0 + 3.0, strip.top() + 3.0),
            vec2(w, strip.height() - 6.0),
        );
        (x0, x1, chip)
    }

    /// The label strip between the ruler and the tracks.
    pub(super) fn label_strip(&mut self, ui: &mut Ui, strip: Rect) {
        let resp = ui.interact(
            strip,
            ui.scope_id().with("label_strip"),
            Sense::click_and_drag(),
        );
        let p = ui.painter_at(strip);
        p.rect_filled(strip, CornerRadius::ZERO, self.pal.strip_bg);
        p.line_segment(
            [
                pos2(strip.left(), strip.bottom() - 0.5),
                pos2(strip.right(), strip.bottom() - 0.5),
            ],
            Stroke::new(1.0, self.pal.strip_sep),
        );
        let mut hits: Vec<(u64, Rect, f32, f32, bool)> = Vec::new();
        for l in &self.project.labels {
            let (x0, x1, chip) = self.label_rect(strip, l, ui);
            if x1 < strip.left() - 200.0 || x0 > strip.right() {
                continue;
            }
            let selected = self.labels_ui.selected == Some(l.id);
            if !l.is_point() {
                let r =
                    Rect::from_min_max(pos2(x0, strip.top() + 2.0), pos2(x1, strip.bottom() - 2.0));
                p.rect_filled(
                    r,
                    CornerRadius::same(2),
                    Color32::from_rgba_unmultiplied(250, 205, 90, if selected { 70 } else { 35 }),
                );
                p.rect_stroke(
                    r,
                    CornerRadius::same(2),
                    Stroke::new(1.0, COL),
                    StrokeKind::Inside,
                );
            } else {
                p.line_segment(
                    [pos2(x0, strip.top()), pos2(x0, strip.bottom())],
                    Stroke::new(1.5, COL),
                );
                p.add(egui::Shape::convex_polygon(
                    vec![
                        pos2(x0 - 4.0, strip.top()),
                        pos2(x0 + 4.0, strip.top()),
                        pos2(x0, strip.top() + 6.0),
                    ],
                    COL,
                    Stroke::NONE,
                ));
            }
            p.rect_filled(
                chip,
                CornerRadius::same(3),
                if selected {
                    Color32::from_rgb(120, 95, 30)
                } else {
                    Color32::from_rgba_unmultiplied(20, 20, 20, 210)
                },
            );
            p.text(
                chip.left_center() + vec2(5.0, 0.0),
                Align2::LEFT_CENTER,
                &l.text,
                FontId::proportional(12.0),
                COL,
            );
            hits.push((l.id, chip, x0, x1, l.is_point()));
        }
        if self.project.labels.is_empty() {
            p.text(
                strip.left_center() + vec2(8.0, 0.0),
                Align2::LEFT_CENTER,
                "Labels",
                FontId::proportional(11.0),
                self.pal.weak_text,
            );
        }

        let hit_at = |pos: egui::Pos2| -> Option<(u64, DragMode)> {
            // Chips first (topmost), then region bodies / point lines.
            for &(id, chip, x0, x1, point) in hits.iter().rev() {
                if !point && (pos.x - x1).abs() <= EDGE_PX {
                    return Some((id, DragMode::End));
                }
                if !point && (pos.x - x0).abs() <= EDGE_PX {
                    return Some((id, DragMode::Start));
                }
                if chip.contains(pos)
                    || (point && (pos.x - x0).abs() <= EDGE_PX)
                    || (!point && pos.x > x0 && pos.x < x1)
                {
                    return Some((id, DragMode::Move));
                }
            }
            None
        };
        if let Some(h) = resp.hover_pos() {
            match hit_at(h) {
                Some((_, DragMode::Start | DragMode::End)) => {
                    ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeHorizontal)
                }
                Some(_) => ui.ctx().set_cursor_icon(egui::CursorIcon::Grab),
                None => {}
            }
        }
        if self.busy() {
            return;
        }
        let sample_at = |app: &Self, x: f32| app.view.x_to_sample(strip, x).max(0.0).round() as i64;
        if resp.drag_started()
            && let Some(pos) = ui.input(|i| i.pointer.press_origin())
            && let Some((id, mode)) = hit_at(pos)
            && let Some(l) = self.project.labels.iter().find(|l| l.id == id)
        {
            let grab = sample_at(self, pos.x) - l.start as i64;
            self.project.push_undo("Move Label");
            self.labels_ui.drag = Some(LabelDrag { id, mode, grab });
            self.labels_ui.selected = Some(id);
        }
        if resp.dragged()
            && let Some(pos) = ui.input(|i| i.pointer.interact_pos())
            && let Some(d) = &self.labels_ui.drag
        {
            let s = sample_at(self, pos.x);
            let (id, mode, grab) = (d.id, d.mode, d.grab);
            if let Some(l) = self.project.labels.iter_mut().find(|l| l.id == id) {
                match mode {
                    DragMode::Move => {
                        let len = l.end - l.start;
                        l.start = (s - grab).max(0) as usize;
                        l.end = l.start + len;
                    }
                    DragMode::Start => l.start = (s.max(0) as usize).min(l.end),
                    DragMode::End => l.end = (s.max(0) as usize).max(l.start),
                }
                let (st, en) = (l.start, l.end);
                self.project.selection = Selection::new(st, en);
                self.project.touch();
            }
        }
        if resp.drag_stopped() {
            self.labels_ui.drag = None;
            labels::sort(&mut self.project.labels);
        }
        if resp.double_clicked()
            && let Some(pos) = resp.interact_pointer_pos()
            && let Some((id, _)) = hit_at(pos)
        {
            self.start_rename(id);
        } else if resp.clicked()
            && let Some(pos) = resp.interact_pointer_pos()
        {
            match hit_at(pos) {
                Some((id, _)) => self.select_label(id),
                None => self.labels_ui.selected = None,
            }
        }
        if resp.secondary_clicked()
            && let Some(pos) = resp.interact_pointer_pos()
        {
            self.labels_ui.menu_for = hit_at(pos).map(|h| h.0);
        }
        if let Some(id) = self.labels_ui.menu_for {
            resp.context_menu(|ui| {
                if ui.button("Rename…").clicked() {
                    self.start_rename(id);
                    ui.close();
                }
                if ui.button("Play").clicked() {
                    self.select_label(id);
                    self.play();
                    ui.close();
                }
                if ui.button("Delete").clicked() {
                    self.delete_label(id);
                    ui.close();
                }
            });
        }
    }

    pub(super) fn select_label(&mut self, id: u64) {
        if let Some(l) = self.project.labels.iter().find(|l| l.id == id) {
            self.project.selection = Selection::new(l.start, l.end);
            self.labels_ui.selected = Some(id);
            self.status = format!("Label “{}”", l.text);
        }
    }

    fn start_rename(&mut self, id: u64) {
        if let Some(l) = self.project.labels.iter().find(|l| l.id == id) {
            self.labels_ui.renaming = Some((id, l.text.clone(), true));
        }
    }

    pub(super) fn delete_label(&mut self, id: u64) {
        if self.project.labels.iter().any(|l| l.id == id) {
            self.project.push_undo("Delete Label");
            self.project.labels.retain(|l| l.id != id);
            if self.labels_ui.selected == Some(id) {
                self.labels_ui.selected = None;
            }
        }
    }

    /// Faint lines through the tracks at each label, so they line up with the audio.
    pub(super) fn draw_label_guides(&self, ui: &Ui, area: Rect) {
        if self.project.labels.is_empty() {
            return;
        }
        let p = ui.painter_at(area);
        for l in &self.project.labels {
            for (k, s) in [l.start, l.end].into_iter().enumerate() {
                if k == 1 && l.is_point() {
                    continue;
                }
                let x = self.view.sample_to_x(area, s as f64);
                if x >= area.left() && x <= area.right() {
                    p.line_segment(
                        [pos2(x, area.top()), pos2(x, area.bottom())],
                        Stroke::new(1.0, Color32::from_rgba_unmultiplied(250, 205, 90, 45)),
                    );
                }
            }
        }
    }

    /// Rename popup and the Labels window.
    pub(super) fn label_windows(&mut self, ctx: &egui::Context) {
        if let Some((id, mut text, first)) = self.labels_ui.renaming.take() {
            let mut done = None;
            egui::Window::new("Label name")
                .collapsible(false)
                .resizable(false)
                .anchor(Align2::CENTER_TOP, vec2(0.0, 120.0))
                .show(ctx, |ui| {
                    let r = ui.add(egui::TextEdit::singleline(&mut text).desired_width(300.0));
                    if first {
                        r.request_focus();
                        if let Some(mut st) = egui::TextEdit::load_state(ui.ctx(), r.id) {
                            let all = egui::text::CCursorRange::two(
                                egui::text::CCursor::new(0),
                                egui::text::CCursor::new(text.chars().count()),
                            );
                            st.cursor.set_char_range(Some(all));
                            st.store(ui.ctx(), r.id);
                        }
                    }
                    let enter = r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                    ui.horizontal(|ui| {
                        if ui.button("OK").clicked() || enter {
                            done = Some(true);
                        }
                        if ui.button("Cancel").clicked()
                            || ui.input(|i| i.key_pressed(egui::Key::Escape))
                        {
                            done = Some(false);
                        }
                    });
                });
            match done {
                Some(true) => {
                    if let Some(l) = self.project.labels.iter().find(|l| l.id == id)
                        && l.text != text
                    {
                        self.project.push_undo("Rename Label");
                        if let Some(l) = self.project.labels.iter_mut().find(|l| l.id == id) {
                            l.text = text.trim().to_string();
                        }
                    }
                }
                Some(false) => {}
                None => self.labels_ui.renaming = Some((id, text, false)),
            }
        }

        if !self.labels_ui.window_open {
            return;
        }
        let mut open = true;
        let mut delete = None;
        let mut play = None;
        let mut import = false;
        let mut export_txt = false;
        let mut export_audio = false;
        let rate = self.project.rate;
        egui::Window::new("Labels")
            .default_width(560.0)
            .open(&mut open)
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    if ui.button("+ Add at selection (Ctrl+B)").clicked() {
                        self.add_label_at_selection();
                    }
                    if ui.button("Import…").on_hover_text("Audacity label file (.txt)").clicked() {
                        import = true;
                    }
                    if ui
                        .add_enabled(!self.project.labels.is_empty(), egui::Button::new("Export labels…"))
                        .on_hover_text("Audacity label file (.txt)")
                        .clicked()
                    {
                        export_txt = true;
                    }
                });
                ui.separator();
                if self.project.labels.is_empty() {
                    ui.label(
                        RichText::new(
                            "No labels yet. Select some audio (or click a spot) and press Ctrl+B. \
                             Labels mark chapters, songs or segments; they become chapters in exported \
                             files and can be exported as separate files.",
                        )
                        .weak(),
                    );
                }
                let fmt_t = |s: usize| {
                    let t = s as f64 / rate.max(1) as f64;
                    format!("{}:{:06.3}", (t / 60.0) as u64, t % 60.0)
                };
                egui::ScrollArea::vertical().max_height(320.0).show(ui, |ui| {
                    egui::Grid::new("labels_grid").num_columns(5).striped(true).spacing([8.0, 4.0]).show(ui, |ui| {
                        let mut changed = false;
                        for (i, l) in self.project.labels.iter_mut().enumerate() {
                            ui.label(format!("{}", i + 1));
                            ui.monospace(if l.is_point() { fmt_t(l.start) } else { format!("{} – {}", fmt_t(l.start), fmt_t(l.end)) });
                            if ui.add(egui::TextEdit::singleline(&mut l.text).desired_width(240.0)).changed() {
                                changed = true;
                            }
                            if ui.small_button("▶").on_hover_text("Select and play").clicked() {
                                play = Some(l.id);
                            }
                            if ui.small_button("🗑").on_hover_text("Delete").clicked() {
                                delete = Some(l.id);
                            }
                            ui.end_row();
                        }
                        if changed {
                            self.project.touch();
                        }
                    });
                });
                ui.separator();
                egui::CollapsingHeader::new("Export audio per label")
                    .default_open(true)
                    .show(ui, |ui| {
                        ui.label(
                            RichText::new(
                                "Region labels are exported as they are; point labels split the audio from one \
                                 label to the next. Files are named after the labels and tagged with the \
                                 project's metadata and track numbers.",
                            )
                            .small()
                            .weak(),
                        );
                        egui::Grid::new("lbl_export").num_columns(2).spacing([12.0, 6.0]).show(ui, |ui| {
                            Self::export_format_ui(ui, &mut self.labels_ui.export_fmt);
                        });
                        ui.checkbox(&mut self.labels_ui.before_first, "Also export the audio before the first label (“Intro”)");
                        ui.checkbox(&mut self.labels_ui.numbered, "Number the files (01 - …, 02 - …)");
                        ui.checkbox(&mut self.tag_opts.tags, "Tag files with the project's metadata");
                        let n = labels::export_regions(&self.project.labels, self.project.len(), self.labels_ui.before_first).len();
                        if ui
                            .add_enabled(n > 0 && !self.busy(), egui::Button::new(format!("Export {n} file(s) to folder…")))
                            .clicked()
                        {
                            export_audio = true;
                        }
                    });
            });
        if !open {
            self.labels_ui.window_open = false;
        }
        if let Some(id) = delete {
            self.delete_label(id);
        }
        if let Some(id) = play {
            self.select_label(id);
            self.play();
        }
        if import
            && let Some(path) = rfd::FileDialog::new()
                .add_filter("Audacity labels", &["txt"])
                .pick_file()
        {
            match std::fs::read_to_string(&path)
                .map_err(|e| e.to_string())
                .and_then(|t| labels::parse_audacity_txt(&t, rate))
            {
                Ok(list) if !list.is_empty() => {
                    self.project.push_undo("Import Labels");
                    for (s, e, t) in &list {
                        let id = self.project.alloc_id();
                        self.project.labels.push(labels::Label {
                            id,
                            start: *s,
                            end: *e,
                            text: t.clone(),
                        });
                    }
                    labels::sort(&mut self.project.labels);
                    self.status = format!("Imported {} labels", list.len());
                }
                Ok(_) => self.status = "That file has no labels".into(),
                Err(e) => {
                    self.dialog = Some(Dialog::Message(format!("Could not read the labels:\n{e}")))
                }
            }
        }
        if export_txt
            && let Some(path) = rfd::FileDialog::new()
                .add_filter("Audacity labels", &["txt"])
                .set_file_name(format!("{} labels.txt", self.title))
                .save_file()
        {
            match std::fs::write(&path, labels::to_audacity_txt(&self.project.labels, rate)) {
                Ok(()) => self.status = format!("Saved {}", path.display()),
                Err(e) => self.dialog = Some(Dialog::Message(format!("Could not save:\n{e}"))),
            }
        }
        if export_audio && let Some(dir) = rfd::FileDialog::new().pick_folder() {
            let (fmt, bf, num) = (
                self.labels_ui.export_fmt,
                self.labels_ui.before_first,
                self.labels_ui.numbered,
            );
            if let Err(e) = self.start_export_many(dir, fmt, bf, num) {
                self.dialog = Some(Dialog::Message(e));
            }
        }
    }
}
