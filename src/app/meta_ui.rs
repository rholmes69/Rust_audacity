//! File ▸ Metadata…: title, artist, album, cover art… written into exported files.

use eframe::egui::{self, RichText};

use super::{AudacityApp, Dialog};
use crate::metadata::{self, Metadata};

const GENRES: [&str; 14] = [
    "Podcast",
    "Speech",
    "Audiobook",
    "Rock",
    "Pop",
    "Hip-Hop",
    "Electronic",
    "Jazz",
    "Classical",
    "Country",
    "Folk",
    "Soundtrack",
    "Ambient",
    "Other",
];

impl AudacityApp {
    pub(super) fn metadata_window(&mut self, ctx: &egui::Context) {
        if !self.meta_open {
            return;
        }
        let mut open = true;
        let before = self.project.metadata.clone();
        let mut choose_cover = false;
        egui::Window::new("Metadata")
            .collapsible(false)
            .resizable(false)
            .default_width(460.0)
            .open(&mut open)
            .show(ctx, |ui| {
                ui.label(
                    RichText::new(
                        "Written into exported MP3, FLAC, Ogg and WAV files (cover art: MP3/FLAC/Ogg). \
                         Saved with the project.",
                    )
                    .weak(),
                );
                ui.add_space(4.0);
                let m = &mut self.project.metadata;
                egui::Grid::new("meta").num_columns(2).spacing([12.0, 6.0]).show(ui, |ui| {
                    let row = |ui: &mut egui::Ui, label: &str, v: &mut String, hint: &str| {
                        ui.label(label);
                        ui.add(egui::TextEdit::singleline(v).hint_text(hint).desired_width(300.0));
                        ui.end_row();
                    };
                    row(ui, "Title:", &mut m.title, "Song or episode title");
                    row(ui, "Artist:", &mut m.artist, "");
                    row(ui, "Album / show:", &mut m.album, "");
                    row(ui, "Album artist:", &mut m.album_artist, "");
                    row(ui, "Year:", &mut m.year, "e.g. 2026");
                    row(ui, "Track:", &mut m.track, "e.g. 3 or 3/12");
                    ui.label("Genre:");
                    ui.horizontal(|ui| {
                        ui.add(egui::TextEdit::singleline(&mut m.genre).desired_width(200.0));
                        egui::ComboBox::from_id_salt("genre")
                            .selected_text("Pick…")
                            .show_ui(ui, |ui| {
                                for g in GENRES {
                                    if ui.selectable_label(m.genre == g, g).clicked() {
                                        m.genre = g.to_string();
                                    }
                                }
                            });
                    });
                    ui.end_row();
                    ui.label("Comment:");
                    ui.add(egui::TextEdit::multiline(&mut m.comment).desired_rows(3).desired_width(300.0));
                    ui.end_row();
                    ui.label("Cover art:");
                    ui.horizontal(|ui| {
                        match &m.cover {
                            Some(c) => {
                                let dims = metadata::image_info(&c.data)
                                    .map(|(_, w, h)| if w > 0 { format!("{w}×{h}, ") } else { String::new() })
                                    .unwrap_or_default();
                                ui.label(format!(
                                    "{} ({dims}{} KB)",
                                    if c.mime == "image/png" { "PNG" } else { "JPEG" },
                                    c.data.len() / 1024
                                ));
                                if ui.small_button("Remove").clicked() {
                                    m.cover = None;
                                }
                            }
                            None => {
                                ui.label(RichText::new("none").weak());
                            }
                        }
                        if ui.button("Choose image…").clicked() {
                            choose_cover = true;
                        }
                    });
                    ui.end_row();
                });
                ui.label(
                    RichText::new("Square images of 1400–3000 px are what podcast directories expect.")
                        .small()
                        .weak(),
                );
                ui.separator();
                ui.horizontal(|ui| {
                    if ui.button("Clear all").clicked() {
                        self.project.metadata = Metadata::default();
                    }
                    if !self.project.labels.is_empty() {
                        ui.label(
                            RichText::new(format!(
                                "{} label(s) will be written as chapters (Export ▸ “Write labels as chapters”).",
                                self.project.labels.len()
                            ))
                            .small(),
                        );
                    }
                });
            });
        if choose_cover
            && let Some(path) = rfd::FileDialog::new()
                .add_filter("Images", &["png", "jpg", "jpeg"])
                .pick_file()
        {
            match metadata::load_cover(&path) {
                Ok(c) => self.project.metadata.cover = Some(c),
                Err(e) => self.dialog = Some(Dialog::Message(format!("{e:#}"))),
            }
        }
        if self.project.metadata != before {
            self.project.dirty = true;
        }
        if !open {
            self.meta_open = false;
        }
    }
}
