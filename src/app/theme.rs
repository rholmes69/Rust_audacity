//! Light / dark / follow-the-system appearance, and the colours the timeline uses.

use eframe::egui::{self, Color32};
use serde::{Deserialize, Serialize};

const KEY_THEME: &str = "theme";

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum ThemeChoice {
    #[default]
    Dark,
    Light,
    /// Follow the operating system's light/dark setting.
    System,
}

impl ThemeChoice {
    pub const ALL: [ThemeChoice; 3] = [ThemeChoice::Dark, ThemeChoice::Light, ThemeChoice::System];

    pub fn label(self) -> &'static str {
        match self {
            ThemeChoice::Dark => "Dark",
            ThemeChoice::Light => "Light",
            ThemeChoice::System => "Same as system",
        }
    }

    pub fn load(storage: Option<&dyn eframe::Storage>) -> Self {
        storage
            .and_then(|s| eframe::get_value(s, KEY_THEME))
            .unwrap_or_default()
    }

    pub fn save(self, storage: &mut dyn eframe::Storage) {
        eframe::set_value(storage, KEY_THEME, &self);
    }

    pub fn apply(self, ctx: &egui::Context) {
        let pref = match self {
            ThemeChoice::Dark => egui::ThemePreference::Dark,
            ThemeChoice::Light => egui::ThemePreference::Light,
            ThemeChoice::System => egui::ThemePreference::System,
        };
        ctx.set_theme(pref);
    }
}

/// Timeline colours for the current theme.
#[derive(Clone, Copy, Debug)]
pub struct Palette {
    pub ruler_bg: Color32,
    pub ruler_tick: Color32,
    pub ruler_text: Color32,
    pub panel_bg: Color32,
    pub panel_bg_sel: Color32,
    pub track_bg: Color32,
    pub track_bg_sel: Color32,
    /// Shading after the end of a track's audio.
    pub beyond_end: Color32,
    pub wave: Color32,
    pub rms: Color32,
    pub center: Color32,
    pub lane_sep: Color32,
    pub row_sep: Color32,
    pub meter_bg: Color32,
    pub strip_bg: Color32,
    pub strip_sep: Color32,
    pub scroll_bg: Color32,
    pub thumb: Color32,
    pub thumb_hot: Color32,
    pub weak_text: Color32,
    pub cursor: Color32,
    pub time_text: Color32,
}

impl Palette {
    pub fn for_mode(dark: bool) -> Self {
        if dark {
            Self {
                ruler_bg: Color32::from_gray(38),
                ruler_tick: Color32::from_gray(150),
                ruler_text: Color32::from_gray(200),
                panel_bg: Color32::from_gray(42),
                panel_bg_sel: Color32::from_rgb(52, 58, 72),
                track_bg: Color32::from_gray(28),
                track_bg_sel: Color32::from_rgb(34, 38, 48),
                beyond_end: Color32::from_gray(22),
                wave: Color32::from_rgb(100, 140, 230),
                rms: Color32::from_rgb(150, 185, 255),
                center: Color32::from_gray(70),
                lane_sep: Color32::from_gray(50),
                row_sep: Color32::from_gray(55),
                meter_bg: Color32::from_gray(30),
                strip_bg: Color32::from_gray(30),
                strip_sep: Color32::from_gray(50),
                scroll_bg: Color32::from_gray(35),
                thumb: Color32::from_gray(100),
                thumb_hot: Color32::from_gray(140),
                weak_text: Color32::from_gray(140),
                cursor: Color32::WHITE,
                time_text: Color32::from_rgb(120, 200, 255),
            }
        } else {
            Self {
                ruler_bg: Color32::from_gray(222),
                ruler_tick: Color32::from_gray(95),
                ruler_text: Color32::from_gray(40),
                panel_bg: Color32::from_gray(218),
                panel_bg_sel: Color32::from_rgb(198, 210, 236),
                track_bg: Color32::from_gray(246),
                track_bg_sel: Color32::from_rgb(232, 238, 250),
                beyond_end: Color32::from_gray(226),
                wave: Color32::from_rgb(45, 85, 185),
                rms: Color32::from_rgb(105, 145, 230),
                center: Color32::from_gray(185),
                lane_sep: Color32::from_gray(200),
                row_sep: Color32::from_gray(190),
                meter_bg: Color32::from_gray(200),
                strip_bg: Color32::from_gray(232),
                strip_sep: Color32::from_gray(200),
                scroll_bg: Color32::from_gray(215),
                thumb: Color32::from_gray(160),
                thumb_hot: Color32::from_gray(125),
                weak_text: Color32::from_gray(110),
                cursor: Color32::BLACK,
                time_text: Color32::from_rgb(20, 90, 170),
            }
        }
    }
}
