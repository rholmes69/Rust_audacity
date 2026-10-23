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
    batch, bridge, dsp, effects, envelope, fileio, labels, mcp, metadata, ollama, plugins, project,
    projectfile, recovery, track, transcript, whisper,
};

use std::path::PathBuf;

fn main() -> eframe::Result {
    // Plugin scanning runs in a child process so a crashing plugin can't take the app down.
    let args: Vec<std::ffi::OsString> = std::env::args_os().collect();
    if args.get(1).is_some_and(|a| a == "--scan-plugin") {
        let path = PathBuf::from(args.get(2).cloned().unwrap_or_default());
        match plugins::scan_file(&path) {
            Ok(list) => println!(
                "{}",
                serde_json::to_string(&list).unwrap_or_else(|_| "[]".into())
            ),
            Err(e) => {
                eprintln!("{e:#}");
                println!("[]");
            }
        }
        // Skip plugin library teardown, which some plugins get wrong.
        std::process::exit(0);
    }
    let files: Vec<PathBuf> = args.into_iter().skip(1).map(PathBuf::from).collect();
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
