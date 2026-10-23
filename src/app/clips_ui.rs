//! Clips on the timeline: headers with names, gaps between clips, dragging a clip (by its
//! header, or anywhere with the Time Shift tool) and the clip context menu.

use eframe::egui::{
    self, Align2, Color32, CornerRadius, FontId, Painter, Rect, Sense, Stroke, StrokeKind, Ui, pos2,
};

use super::AudacityApp;
use crate::project::Selection;

pub const HEADER_H: f32 = 16.0;

/// A clip being dragged.
pub struct ClipDrag {
    pub track: usize,
    pub clip: usize,
    start_x: f32,
    /// Where it would land (already moved out of the way of other clips).
    target_start: usize,
    dest: usize,
}

#[derive(Default)]
pub struct ClipsState {
    pub drag: Option<ClipDrag>,
    /// (track, clip, edited name)
    rename: Option<(usize, usize, String)>,
}

impl AudacityApp {
    /// Shade gaps and draw clip headers and edges over the waveform.
    pub(super) fn draw_clips(&self, painter: &Painter, i: usize, rect: Rect, draw_left: f64) {
        let t = &self.project.tracks[i];
        let to_x = |s: usize| rect.left() + ((s as f64 - draw_left) / self.view.spp) as f32;
        // Gaps: everything that isn't inside a clip.
        let mut prev = 0usize;
        let gap_fill = Color32::from_black_alpha(90);
        for c in t
            .clips
            .iter()
            .map(|c| (c.start, c.end))
            .chain(std::iter::once((usize::MAX, usize::MAX)))
        {
            if c.0 > prev {
                let x0 = to_x(prev).max(rect.left());
                let x1 = if c.0 == usize::MAX {
                    rect.right()
                } else {
                    to_x(c.0).min(rect.right())
                };
                if x1 > x0 {
                    painter.rect_filled(
                        Rect::from_min_max(pos2(x0, rect.top()), pos2(x1, rect.bottom())),
                        CornerRadius::ZERO,
                        gap_fill,
                    );
                }
            }
            prev = c.1;
        }
        let sel = self.project.selection;
        for (ci, c) in t.clips.iter().enumerate() {
            let (x0, x1) = (to_x(c.start), to_x(c.end));
            if x1 < rect.left() || x0 > rect.right() {
                continue;
            }
            let selected = t.selected && sel.start == c.start && sel.end == c.end;
            let dragging = self
                .clips
                .drag
                .as_ref()
                .is_some_and(|d| d.track == i && d.clip == ci);
            let header = Rect::from_min_max(pos2(x0, rect.top()), pos2(x1, rect.top() + HEADER_H));
            let fill = if dragging {
                Color32::from_rgba_unmultiplied(255, 210, 90, 70)
            } else if selected {
                Color32::from_rgba_unmultiplied(140, 170, 255, 120)
            } else {
                Color32::from_rgba_unmultiplied(90, 110, 160, 110)
            };
            painter.rect_filled(header, CornerRadius::ZERO, fill);
            let edge = Stroke::new(1.0, Color32::from_rgba_unmultiplied(170, 190, 240, 140));
            painter.line_segment([pos2(x0, rect.top()), pos2(x0, rect.bottom())], edge);
            painter.line_segment([pos2(x1, rect.top()), pos2(x1, rect.bottom())], edge);
            if x1 - x0 > 24.0 {
                let text_x = x0.max(rect.left()) + 4.0;
                let clip_text = painter.with_clip_rect(header.intersect(rect));
                clip_text.text(
                    pos2(text_x, header.center().y),
                    Align2::LEFT_CENTER,
                    &c.name,
                    FontId::proportional(11.0),
                    Color32::from_gray(235),
                );
            }
        }
        // Where a dragged clip will land.
        if let Some(d) = &self.clips.drag
            && d.dest == i
            && let Some(c) = self.project.tracks[d.track].clips.get(d.clip)
        {
            let x0 = to_x(d.target_start);
            let x1 = to_x(d.target_start + c.len());
            let ghost =
                Rect::from_min_max(pos2(x0, rect.top() + 1.0), pos2(x1, rect.bottom() - 1.0));
            painter.rect_filled(
                ghost,
                CornerRadius::same(3),
                Color32::from_rgba_unmultiplied(255, 210, 90, 35),
            );
            painter.rect_stroke(
                ghost,
                CornerRadius::same(3),
                Stroke::new(1.5, Color32::from_rgb(255, 210, 90)),
                StrokeKind::Inside,
            );
        }
    }

    /// Clip headers: drag to move, click to select, double-click to rename, right-click
    /// for the clip menu. Returns true if the pointer is on a header (so the track's own
    /// interaction should be skipped).
    pub(super) fn clip_headers(
        &mut self,
        ui: &mut Ui,
        i: usize,
        rect: Rect,
        draw_left: f64,
    ) -> bool {
        let n = self.project.tracks[i].clips.len();
        let mut on_header = false;
        for ci in 0..n {
            let c = &self.project.tracks[i].clips[ci];
            let to_x = |s: usize| rect.left() + ((s as f64 - draw_left) / self.view.spp) as f32;
            let (x0, x1) = (
                to_x(c.start).max(rect.left()),
                to_x(c.end).min(rect.right()),
            );
            if x1 - x0 < 2.0 {
                continue;
            }
            let header = Rect::from_min_max(pos2(x0, rect.top()), pos2(x1, rect.top() + HEADER_H));
            let id = ui.scope_id().with(("clip", self.project.tracks[i].id, ci));
            let r = ui.interact(header, id, Sense::click_and_drag());
            on_header |= r.hovered() || r.dragged();
            if r.hovered() && self.clips.drag.is_none() {
                ui.ctx().set_cursor_icon(egui::CursorIcon::Grab);
            }
            if r.drag_started()
                && let Some(p) = ui.input(|inp| inp.pointer.press_origin())
            {
                self.start_clip_drag(i, ci, p.x);
            }
            if r.clicked() {
                self.select_clip(i, ci);
            }
            if r.double_clicked() {
                let name = self.project.tracks[i].clips[ci].name.clone();
                self.clips.rename = Some((i, ci, name));
            }
            r.context_menu(|ui| self.clip_menu(ui, i, ci));
        }
        on_header
    }

    pub(super) fn start_clip_drag(&mut self, track: usize, clip: usize, x: f32) {
        let start = self.project.tracks[track].clips[clip].start;
        self.clips.drag = Some(ClipDrag {
            track,
            clip,
            start_x: x,
            target_start: start,
            dest: track,
        });
    }

    fn select_clip(&mut self, i: usize, ci: usize) {
        let c = &self.project.tracks[i].clips[ci];
        self.project.selection = Selection::new(c.start, c.end);
        for (j, t) in self.project.tracks.iter_mut().enumerate() {
            t.selected = j == i;
        }
    }

    fn clip_menu(&mut self, ui: &mut Ui, i: usize, ci: usize) {
        let Some(c) = self
            .project
            .tracks
            .get(i)
            .and_then(|t| t.clips.get(ci))
            .cloned()
        else {
            ui.close();
            return;
        };
        ui.label(egui::RichText::new(&c.name).strong());
        ui.separator();
        if ui.button("Select clip").clicked() {
            self.select_clip(i, ci);
            ui.close();
        }
        if ui.button("Rename…").clicked() {
            self.clips.rename = Some((i, ci, c.name.clone()));
            ui.close();
        }
        if ui.button("▶ Play clip").clicked() {
            self.select_clip(i, ci);
            self.engine.stop_playback();
            self.play();
            ui.close();
        }
        let cursor = self.project.selection.start;
        let can_split = self.project.selection.is_empty() && c.start < cursor && cursor < c.end;
        if ui
            .add_enabled(can_split, egui::Button::new("Split at cursor"))
            .on_disabled_hover_text("Click inside the clip first to place the cursor")
            .clicked()
        {
            self.project.push_undo("Split Clip");
            self.project.tracks[i].split_clip_at(cursor);
            ui.close();
        }
        let next = self.project.tracks[i].clips.get(ci + 1).cloned();
        if ui
            .add_enabled(next.is_some(), egui::Button::new("Join with next clip"))
            .clicked()
            && let Some(nx) = next
        {
            self.project.push_undo("Join Clips");
            self.project.tracks[i].join_clips(c.start, nx.start);
            ui.close();
        }
        ui.separator();
        if ui.button("🗑 Delete clip").clicked() && !self.busy() {
            self.engine.stop_playback();
            self.project.delete_clip(i, ci);
            ui.close();
        }
    }

    /// Follow the pointer while a clip is dragged, and move it on release.
    pub(super) fn update_clip_drag(&mut self, ui: &Ui) {
        let Some(d) = &self.clips.drag else { return };
        let (down, pos) = ui.input(|i| (i.pointer.primary_down(), i.pointer.interact_pos()));
        let (track, clip) = (d.track, d.clip);
        let Some(c) = self
            .project
            .tracks
            .get(track)
            .and_then(|t| t.clips.get(clip))
            .cloned()
        else {
            self.clips.drag = None;
            return;
        };
        if down {
            if let Some(p) = pos {
                let delta = ((p.x - d.start_x) as f64 * self.view.spp).round() as i64;
                let dest = self
                    .prev_track_rects
                    .iter()
                    .find(|(_, r)| p.y >= r.top() && p.y <= r.bottom())
                    .map_or(track, |(ti, _)| *ti);
                let skip = (dest == track).then_some(clip);
                let target = self
                    .project
                    .clip_slot(dest, c.start as i64 + delta, c.len(), skip);
                if let Some(d) = &mut self.clips.drag {
                    d.target_start = target;
                    d.dest = dest;
                }
                self.status = format!(
                    "Moving clip “{}” by {:+.3} s{}",
                    c.name,
                    (target as f64 - c.start as f64) / self.rate(),
                    if dest != track {
                        format!(" → “{}”", self.project.tracks[dest].name)
                    } else {
                        String::new()
                    }
                );
            }
            ui.ctx().set_cursor_icon(egui::CursorIcon::Grabbing);
            return;
        }
        let d = self.clips.drag.take().expect("checked above");
        let delta = d.target_start as i64 - c.start as i64;
        if delta == 0 && d.dest == track {
            self.select_clip(track, clip);
            return;
        }
        self.engine.stop_playback();
        let dest = (d.dest != track).then_some(d.dest);
        if self.project.move_clip(track, clip, delta, dest) {
            self.status = format!(
                "Moved clip “{}” by {:+.3} s",
                c.name,
                delta as f64 / self.rate()
            );
        }
    }

    pub(super) fn clip_rename_window(&mut self, ctx: &egui::Context) {
        let Some((ti, ci, mut name)) = self.clips.rename.take() else {
            return;
        };
        let mut open = true;
        let mut done = false;
        let mut cancel = false;
        egui::Window::new("Rename Clip")
            .collapsible(false)
            .resizable(false)
            .anchor(Align2::CENTER_CENTER, [0.0, 0.0])
            .open(&mut open)
            .show(ctx, |ui| {
                let r = ui.add(egui::TextEdit::singleline(&mut name).desired_width(260.0));
                if ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                    done = true;
                } else if !r.has_focus() && !r.lost_focus() {
                    r.request_focus();
                }
                ui.horizontal(|ui| {
                    if ui.button("OK").clicked() {
                        done = true;
                    }
                    if ui.button("Cancel").clicked() {
                        cancel = true;
                    }
                });
            });
        if done {
            self.project.rename_clip(ti, ci, name.trim());
        } else if open && !cancel {
            self.clips.rename = Some((ti, ci, name));
        }
    }

    /// Edit ▸ Clip menu entries.
    pub(super) fn clip_edit_menu(&mut self, ui: &mut Ui) {
        ui.menu_button("Clip", |ui| {
            let idle = !self.busy();
            if ui
                .add_enabled(
                    idle,
                    egui::Button::new("Split")
                        .shortcut_text(self.keys.label(super::keys::Act::SplitClip)),
                )
                .on_hover_text("Split clips at the cursor, or at both ends of the selection")
                .clicked()
            {
                self.do_edit(crate::project::Project::split_clips, "Split Clip");
                ui.close();
            }
            if ui
                .add_enabled(
                    idle,
                    egui::Button::new("Join")
                        .shortcut_text(self.keys.label(super::keys::Act::JoinClips)),
                )
                .on_hover_text("Join the clips the selection touches into one")
                .clicked()
            {
                self.do_edit(crate::project::Project::join_clips, "Join Clips");
                ui.close();
            }
            let at = self.clip_at_cursor();
            if ui
                .add_enabled(at.is_some(), egui::Button::new("Rename…"))
                .clicked()
                && let Some((ti, ci)) = at
            {
                let name = self.project.tracks[ti].clips[ci].name.clone();
                self.clips.rename = Some((ti, ci, name));
                ui.close();
            }
            if ui
                .add_enabled(at.is_some() && idle, egui::Button::new("Delete Clip"))
                .clicked()
                && let Some((ti, ci)) = at
            {
                self.engine.stop_playback();
                self.project.delete_clip(ti, ci);
                ui.close();
            }
        });
    }

    /// The clip under the cursor on the first selected track.
    fn clip_at_cursor(&self) -> Option<(usize, usize)> {
        let pos = self.project.selection.start;
        let ti = self.project.target_tracks().into_iter().next()?;
        Some((ti, self.project.tracks[ti].clip_at(pos)?))
    }
}
