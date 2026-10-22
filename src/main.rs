//! Rust Audacity — a small Audacity-style audio editor.
#![cfg_attr(
    all(not(debug_assertions), target_os = "windows"),
    windows_subsystem = "windows"
)]

mod app;
mod engine;
mod spectrogram;
mod ui_effects;
mod waveform;

// The audio engine lives in the library crate so the MCP server can share it.
use rust_audacity::{
    bridge, dsp, effects, fileio, project, projectfile, track, transcript, whisper,
};

use std::path::PathBuf;

fn main() -> eframe::Result {
    let files: Vec<PathBuf> = std::env::args_os().skip(1).map(PathBuf::from).collect();
    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_title("Rust Audacity")
            .with_icon(
                eframe::icon_data::from_png_bytes(include_bytes!(
                    "../assets/rust_audacity_icon_256.png"
                ))
                .unwrap_or_default(),
            )
            .with_inner_size([1280.0, 760.0])
            .with_min_inner_size([760.0, 420.0])
            .with_drag_and_drop(true),
        ..Default::default()
    };
    eframe::run_native(
        "Rust Audacity",
        options,
        Box::new(|cc| Ok(Box::new(app::AudacityApp::new(cc, files)))),
    )
}

#[cfg(test)]
mod icon_tests {
    #[test]
    fn app_icon_decodes() {
        let icon = eframe::icon_data::from_png_bytes(include_bytes!(
            "../assets/rust_audacity_icon_256.png"
        ))
        .unwrap();
        assert_eq!((icon.width, icon.height), (256, 256));
    }
}
