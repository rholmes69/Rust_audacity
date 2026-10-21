//! Spectrogram texture cache for the track view (pixels come from `dsp::specgram`).

use std::collections::HashMap;

use eframe::egui::{self, Color32, ColorImage, TextureHandle, TextureOptions};

pub use crate::dsp::specgram::frac_of;

pub fn render(
    data: &[f32],
    rate: u32,
    left: f64,
    spp: f64,
    width: usize,
    height: usize,
) -> ColorImage {
    let px = crate::dsp::specgram::render(data, rate, left, spp, width, height);
    ColorImage::new(
        [width.max(1), height.max(1)],
        px.into_iter()
            .map(|[r, g, b]| Color32::from_rgb(r, g, b))
            .collect(),
    )
}

#[derive(PartialEq, Clone, Copy)]
struct Key {
    revision: u64,
    len: usize,
    left: u64,
    spp: u64,
    w: usize,
    h: usize,
}

#[derive(Default)]
pub struct SpectrogramCache {
    map: HashMap<(u64, usize), (Key, TextureHandle)>,
}

impl SpectrogramCache {
    #[allow(clippy::too_many_arguments)]
    pub fn texture(
        &mut self,
        ctx: &egui::Context,
        track_id: u64,
        ch: usize,
        revision: u64,
        data: &[f32],
        rate: u32,
        left: f64,
        spp: f64,
        w: usize,
        h: usize,
    ) -> &TextureHandle {
        let key = Key {
            revision,
            len: data.len(),
            left: left.to_bits(),
            spp: spp.to_bits(),
            w,
            h,
        };
        let fresh = !matches!(self.map.get(&(track_id, ch)), Some((k, _)) if *k == key);
        if fresh {
            let img = render(data, rate, left, spp, w, h);
            match self.map.get_mut(&(track_id, ch)) {
                Some((k, tex)) => {
                    tex.set(img, TextureOptions::LINEAR);
                    *k = key;
                }
                None => {
                    let tex = ctx.load_texture(
                        format!("spec-{track_id}-{ch}"),
                        img,
                        TextureOptions::LINEAR,
                    );
                    self.map.insert((track_id, ch), (key, tex));
                }
            }
        }
        &self.map[&(track_id, ch)].1
    }

    pub fn retain_tracks(&mut self, ids: &[u64]) {
        self.map.retain(|(id, _), _| ids.contains(id));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tone_lights_up_its_row() {
        let rate = 44100;
        let x: Vec<f32> = (0..44100)
            .map(|i| (2.0 * std::f32::consts::PI * 1000.0 * i as f32 / rate as f32).sin())
            .collect();
        let h = 200;
        let img = render(&x, rate, 0.0, 100.0, 50, h);
        let row = (frac_of(1000.0, rate) * h as f32) as usize;
        let bright = |c: Color32| c.r() as u32 + c.g() as u32 + c.b() as u32;
        let at_tone = bright(img.pixels[row * 50 + 25]);
        let elsewhere = bright(img.pixels[(h / 10) * 50 + 25]);
        assert!(at_tone > elsewhere + 200, "{at_tone} vs {elsewhere}");
    }
}
