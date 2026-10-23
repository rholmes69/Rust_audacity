//! Plugin hosting against real VST3/CLAP plugins, when some are installed (on Ubuntu:
//! `apt install dragonfly-reverb-vst3 dragonfly-reverb-clap dpf-plugins-clap`). Skipped
//! otherwise.

use rust_audacity::{
    dsp::realtime::{LiveFx, render},
    plugins::*,
};

fn find(name: &str, format: PluginFormat) -> Option<PluginDesc> {
    plugin_files(&default_dirs())
        .iter()
        .filter(|f| f.extension().and_then(|e| e.to_str()) == Some(format.extension()))
        .filter_map(|f| scan_file(f).ok())
        .flatten()
        .find(|d| d.name == name && d.format == format)
}

#[test]
fn vst3_and_clap_process_save_and_restore() {
    let mut tested = 0;
    for format in [PluginFormat::Vst3, PluginFormat::Clap] {
        let Some(desc) = find("Dragonfly Room Reverb", format) else {
            eprintln!("skipping {format:?}: Dragonfly Room Reverb not installed");
            continue;
        };
        tested += 1;
        let mut p = LoadedPlugin::load(&desc, 48000.0).expect("load");
        let params = p.params();
        let dry = params
            .iter()
            .find(|q| q.name == "Dry Level")
            .expect("Dry Level param")
            .clone();
        // An impulse gets a reverb tail.
        let mut buf = vec![vec![0f32; 48000], vec![0f32; 48000]];
        buf[0][0] = 1.0;
        buf[1][0] = 1.0;
        p.processor().lock().unwrap().process(&mut buf);
        let tail: f32 = buf[0][2000..].iter().map(|x| x * x).sum();
        assert!(tail > 1e-4, "{format:?}: no reverb tail ({tail})");
        // Settings survive save → fresh instance → load.
        let target = dry.min + (dry.max - dry.min) * 0.3;
        p.set_param(dry.id, target);
        p.idle();
        let state = p.save_state().unwrap();
        drop(p);
        let mut q = LoadedPlugin::load(&desc, 48000.0).unwrap();
        q.load_state(&state).unwrap();
        let v = q.params().iter().find(|x| x.id == dry.id).unwrap().value;
        assert!(
            (v - target).abs() < 1e-3,
            "{format:?}: restored {v}, expected {target}"
        );
        // Offline rendering (export) through a rack slot uses the saved settings, and a
        // mono track works too.
        let mut slot = LiveFx::plugin(1, desc.clone());
        slot.plugin.as_mut().unwrap().state = state;
        let mono = render(&[slot], vec![vec![0.25f32; 9600]], 48000);
        assert_eq!(mono.len(), 1);
        assert!(mono[0].iter().all(|x| x.is_finite()));
        assert!(mono[0][9000].abs() > 0.01, "{format:?}: dry signal missing");
    }
    eprintln!("tested {tested} plugin format(s)");
}
