//! Waveform rendering with a min/max/RMS summary cache for fast zoomed-out drawing.

use std::collections::HashMap;

use eframe::egui::{Color32, Painter, Pos2, Rect, Stroke, pos2};

pub const BLOCK: usize = 256;

#[derive(Clone, Copy)]
pub struct Block {
    min: f32,
    max: f32,
    sumsq: f32,
}

struct Entry {
    revision: u64,
    len: usize,
    blocks: Vec<Block>,
}

/// Cache keyed by (track id, channel index). Entries are valid for one project
/// revision; within a revision a channel may only grow (live recording), which
/// is handled incrementally.
#[derive(Default)]
pub struct SummaryCache {
    map: HashMap<(u64, usize), Entry>,
}

fn summarize(data: &[f32]) -> Block {
    let mut b = Block {
        min: f32::MAX,
        max: f32::MIN,
        sumsq: 0.0,
    };
    for &x in data {
        b.min = b.min.min(x);
        b.max = b.max.max(x);
        b.sumsq += x * x;
    }
    b
}

impl SummaryCache {
    pub fn get(&mut self, track_id: u64, ch: usize, revision: u64, data: &[f32]) -> &[Block] {
        let e = self.map.entry((track_id, ch)).or_insert(Entry {
            revision: u64::MAX,
            len: 0,
            blocks: Vec::new(),
        });
        if e.revision != revision || data.len() < e.len {
            e.revision = revision;
            e.len = 0;
            e.blocks.clear();
        }
        if data.len() != e.len {
            // Recompute from the last (possibly partial) block onwards.
            let first = e.len / BLOCK;
            e.blocks.truncate(first);
            e.blocks
                .extend(data[first * BLOCK..].chunks(BLOCK).map(summarize));
            e.len = data.len();
        }
        &e.blocks
    }

    /// Drop entries for tracks that no longer exist.
    pub fn retain_tracks(&mut self, ids: &[u64]) {
        self.map.retain(|(id, _), _| ids.contains(id));
    }
}

pub struct Colors {
    pub wave: Color32,
    pub rms: Color32,
    pub clip: Color32,
    pub center: Color32,
}

/// Draw one channel into `rect`.
/// `left` = sample index at rect.left(), `spp` = samples per pixel.
/// `vzoom` scales amplitude (1.0 = full scale fills the lane).
#[allow(clippy::too_many_arguments)]
pub fn draw_channel(
    painter: &Painter,
    rect: Rect,
    data: &[f32],
    blocks: &[Block],
    left: f64,
    spp: f64,
    vzoom: f32,
    colors: &Colors,
) {
    let mid = rect.center().y;
    let half = rect.height() * 0.5 - 1.0;
    let y_of = |v: f32| mid - (v * vzoom).clamp(-1.0, 1.0) * half;
    painter.line_segment(
        [pos2(rect.left(), mid), pos2(rect.right(), mid)],
        Stroke::new(1.0, colors.center),
    );

    let len = data.len();
    if len == 0 {
        return;
    }
    let width = rect.width().ceil() as usize;

    if spp < 1.0 {
        // Zoomed in past one sample per pixel: connect the dots.
        let first = left.floor().max(0.0) as usize;
        let last = ((left + width as f64 * spp).ceil() as usize + 1).min(len);
        if first >= last {
            return;
        }
        let pts: Vec<Pos2> = (first..last)
            .map(|i| {
                pos2(
                    rect.left() + ((i as f64 - left) / spp) as f32,
                    y_of(data[i]),
                )
            })
            .collect();
        painter.add(eframe::egui::Shape::line(
            pts.clone(),
            Stroke::new(1.2, colors.wave),
        ));
        if spp < 0.12 {
            for p in pts {
                painter.circle_filled(p, 2.5, colors.wave);
            }
        }
        return;
    }

    for x in 0..width {
        let s0 = left + x as f64 * spp;
        let s1 = s0 + spp;
        if s1 <= 0.0 {
            continue;
        }
        let a = s0.max(0.0) as usize;
        let b = (s1.ceil() as usize).min(len);
        if a >= len {
            break;
        }
        if a >= b {
            continue;
        }
        let (mn, mx, rms) = if spp >= BLOCK as f64 && !blocks.is_empty() {
            let ba = a / BLOCK;
            let bb = b.div_ceil(BLOCK).min(blocks.len());
            let mut mn = f32::MAX;
            let mut mx = f32::MIN;
            let mut sq = 0f32;
            for bl in &blocks[ba..bb.max(ba + 1).min(blocks.len())] {
                mn = mn.min(bl.min);
                mx = mx.max(bl.max);
                sq += bl.sumsq;
            }
            let n = ((bb - ba).max(1) * BLOCK) as f32;
            (mn, mx, (sq / n).sqrt())
        } else {
            let bl = summarize(&data[a..b]);
            (bl.min, bl.max, (bl.sumsq / (b - a) as f32).sqrt())
        };
        let px = rect.left() + x as f32 + 0.5;
        let clipped = mx >= 0.999 || mn <= -0.999;
        let col = if clipped { colors.clip } else { colors.wave };
        let (y_top, y_bot) = (y_of(mx), y_of(mn));
        painter.line_segment(
            [pos2(px, y_top), pos2(px, (y_bot).max(y_top + 1.0))],
            Stroke::new(1.0, col),
        );
        let r = rms.min(mx.abs().max(mn.abs()));
        let (ry0, ry1) = (y_of(r.min(mx)), y_of((-r).max(mn)));
        if ry1 > ry0 {
            painter.line_segment([pos2(px, ry0), pos2(px, ry1)], Stroke::new(1.0, colors.rms));
        }
    }
}
