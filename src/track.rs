//! Audio track model.
//!
//! Each channel is stored as an `Arc<Vec<f32>>` so that undo snapshots and the
//! playback engine can share sample data cheaply. Edits use `Arc::make_mut`,
//! which copies a channel only if someone else (e.g. the undo history) still
//! holds a reference to it — classic copy-on-write.

use std::sync::Arc;

use crate::envelope::Envelope;

pub type Channel = Arc<Vec<f32>>;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum TrackView {
    #[default]
    Waveform,
    Spectrogram,
}

#[derive(Clone, Debug)]
pub struct Track {
    pub id: u64,
    pub name: String,
    /// One entry per channel (1 = mono, 2 = stereo). All channels have equal length.
    pub channels: Vec<Channel>,
    /// Linear gain applied on playback / mixdown.
    pub gain: f32,
    /// -1.0 (left) ..= 1.0 (right).
    pub pan: f32,
    pub mute: bool,
    pub solo: bool,
    /// Whether the track is part of the current selection.
    pub selected: bool,
    /// Display height in pixels.
    pub height: f32,
    /// Waveform or spectrogram display.
    pub view: TrackView,
    /// Volume automation (empty = flat 0 dB). Applied on playback and mixdown.
    pub envelope: Envelope,
}

impl Track {
    pub fn new(id: u64, name: impl Into<String>, channels: Vec<Vec<f32>>) -> Self {
        assert!(!channels.is_empty(), "a track needs at least one channel");
        let len = channels.iter().map(Vec::len).max().unwrap_or(0);
        let channels = channels
            .into_iter()
            .map(|mut c| {
                c.resize(len, 0.0);
                Arc::new(c)
            })
            .collect();
        Self {
            id,
            name: name.into(),
            channels,
            gain: 1.0,
            pan: 0.0,
            mute: false,
            solo: false,
            selected: true,
            height: 150.0,
            view: TrackView::Waveform,
            envelope: Envelope::default(),
        }
    }

    pub fn empty(id: u64, name: impl Into<String>, num_channels: usize) -> Self {
        Self::new(id, name, vec![Vec::new(); num_channels.max(1)])
    }

    pub fn len(&self) -> usize {
        self.channels.first().map_or(0, |c| c.len())
    }

    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn num_channels(&self) -> usize {
        self.channels.len()
    }

    /// Copy a range of samples out of every channel.
    pub fn extract(&self, start: usize, end: usize) -> Vec<Vec<f32>> {
        let len = self.len();
        let (s, e) = (start.min(len), end.min(len));
        self.channels.iter().map(|c| c[s..e].to_vec()).collect()
    }

    /// Remove a range of samples from every channel (later audio shifts left).
    pub fn remove(&mut self, start: usize, end: usize) {
        if let Some((s, e)) = self.raw_remove(start, end) {
            self.envelope.retime(s, e, s);
        }
    }

    fn raw_remove(&mut self, start: usize, end: usize) -> Option<(usize, usize)> {
        let len = self.len();
        let (s, e) = (start.min(len), end.min(len));
        if s >= e {
            return None;
        }
        for c in &mut self.channels {
            Arc::make_mut(c).drain(s..e);
        }
        Some((s, e))
    }

    /// Insert audio at `pos` (later audio shifts right). Pads with silence if
    /// `pos` is past the end. Channel counts are adapted (mono<->stereo).
    pub fn insert(&mut self, pos: usize, data: &[Vec<f32>]) {
        let len = self.len();
        let n = self.raw_insert(pos, data);
        if n > 0 && pos < len {
            self.envelope.retime(pos, pos, pos + n);
        }
    }

    fn raw_insert(&mut self, pos: usize, data: &[Vec<f32>]) -> usize {
        if data.is_empty() {
            return 0;
        }
        let data = adapt_channels(data, self.num_channels());
        let ins_len = data[0].len();
        if ins_len == 0 {
            return 0;
        }
        let len = self.len();
        for (c, d) in self.channels.iter_mut().zip(data.iter()) {
            let v = Arc::make_mut(c);
            if pos > len {
                v.resize(pos, 0.0);
            }
            let p = pos.min(v.len());
            v.splice(p..p, d.iter().copied());
        }
        ins_len
    }

    /// Overwrite samples in a range with silence.
    pub fn silence(&mut self, start: usize, end: usize) {
        self.process(start, end, |buf| buf.iter_mut().for_each(|s| *s = 0.0));
    }

    /// Apply an in-place operation to each channel's slice in [start, end).
    pub fn process(&mut self, start: usize, end: usize, mut f: impl FnMut(&mut [f32])) {
        let len = self.len();
        let (s, e) = (start.min(len), end.min(len));
        if s >= e {
            return;
        }
        for c in &mut self.channels {
            f(&mut Arc::make_mut(c)[s..e]);
        }
    }

    /// Replace a range with new (possibly different length) audio. Envelope points inside
    /// the range are kept (scaled if the length changes).
    pub fn replace(&mut self, start: usize, end: usize, data: &[Vec<f32>]) {
        let len = self.len();
        let (s, e) = (start.min(len), end.min(len));
        self.raw_remove(s, e);
        let n = self.raw_insert(start, data);
        if start <= len {
            self.envelope.retime(s, e, s + n);
        }
    }

    /// Ensure all channels are at least `len` samples long.
    pub fn pad_to(&mut self, len: usize) {
        if self.len() < len {
            for c in &mut self.channels {
                Arc::make_mut(c).resize(len, 0.0);
            }
        }
    }

    /// Per-channel output gains for left/right after applying gain and pan
    /// (constant-power-ish linear pan, like Audacity's default).
    pub fn lr_gains(&self) -> (f32, f32) {
        let l = if self.pan > 0.0 { 1.0 - self.pan } else { 1.0 };
        let r = if self.pan < 0.0 { 1.0 + self.pan } else { 1.0 };
        (self.gain * l, self.gain * r)
    }
}

/// Convert audio to a target channel count: mono→N duplicates, N→mono averages,
/// stereo→stereo passes through, otherwise channels are wrapped.
pub fn adapt_channels(data: &[Vec<f32>], target: usize) -> Vec<Vec<f32>> {
    let src = data.len();
    if src == target || src == 0 {
        return data.to_vec();
    }
    if target == 1 {
        let len = data[0].len();
        let mut out = vec![0.0; len];
        for ch in data {
            for (o, s) in out.iter_mut().zip(ch) {
                *o += *s;
            }
        }
        let k = 1.0 / src as f32;
        out.iter_mut().for_each(|s| *s *= k);
        return vec![out];
    }
    (0..target).map(|i| data[i % src].clone()).collect()
}

/// Read `ch` at positions `i * step` (linear interpolation), producing `out_len` samples.
pub fn resample_ratio(ch: &[f32], step: f64, out_len: usize) -> Vec<f32> {
    if ch.is_empty() {
        return vec![0.0; out_len];
    }
    (0..out_len)
        .map(|i| {
            let pos = i as f64 * step;
            let i0 = pos.floor() as usize;
            let fr = (pos - i0 as f64) as f32;
            let a = ch[i0.min(ch.len() - 1)];
            let b = ch[(i0 + 1).min(ch.len() - 1)];
            a + (b - a) * fr
        })
        .collect()
}

/// Resample planar audio with linear interpolation.
pub fn resample(data: &[Vec<f32>], from_rate: u32, to_rate: u32) -> Vec<Vec<f32>> {
    if from_rate == to_rate || from_rate == 0 || to_rate == 0 {
        return data.to_vec();
    }
    let ratio = from_rate as f64 / to_rate as f64;
    data.iter()
        .map(|ch| {
            if ch.is_empty() {
                return Vec::new();
            }
            let out_len = ((ch.len() as f64) / ratio).round().max(1.0) as usize;
            (0..out_len)
                .map(|i| {
                    let pos = i as f64 * ratio;
                    let i0 = pos.floor() as usize;
                    let frac = (pos - i0 as f64) as f32;
                    let a = ch[i0.min(ch.len() - 1)];
                    let b = ch[(i0 + 1).min(ch.len() - 1)];
                    a + (b - a) * frac
                })
                .collect()
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insert_remove_roundtrip() {
        let mut t = Track::new(1, "t", vec![vec![1.0, 2.0, 3.0, 4.0]]);
        let cut = t.extract(1, 3);
        t.remove(1, 3);
        assert_eq!(*t.channels[0], vec![1.0, 4.0]);
        t.insert(1, &cut);
        assert_eq!(*t.channels[0], vec![1.0, 2.0, 3.0, 4.0]);
    }

    #[test]
    fn insert_past_end_pads() {
        let mut t = Track::new(1, "t", vec![vec![1.0]]);
        t.insert(3, &[vec![9.0]]);
        assert_eq!(*t.channels[0], vec![1.0, 0.0, 0.0, 9.0]);
    }

    #[test]
    fn copy_on_write_preserves_snapshot() {
        let mut t = Track::new(1, "t", vec![vec![1.0, 2.0]]);
        let snapshot = t.clone();
        t.silence(0, 2);
        assert_eq!(*snapshot.channels[0], vec![1.0, 2.0]);
        assert_eq!(*t.channels[0], vec![0.0, 0.0]);
    }

    #[test]
    fn envelope_follows_edits() {
        use crate::envelope::EnvPoint;
        let mut t = Track::new(1, "t", vec![vec![0.0; 100]]);
        t.envelope = Envelope::from_points(vec![
            EnvPoint { pos: 10, db: 0.0 },
            EnvPoint { pos: 50, db: -12.0 },
        ]);
        t.insert(0, &[vec![0.0; 5]]);
        assert_eq!(t.envelope.points[1].pos, 55);
        // Same-length replace (an effect) keeps the points.
        t.replace(0, 100, &[vec![1.0; 100]]);
        assert_eq!(t.envelope.points[1].pos, 55);
        // Half-length replace (tempo change) scales them.
        t.replace(0, 105, &[vec![1.0; 50]]);
        assert_eq!(t.envelope.points[1].pos, 26);
    }

    #[test]
    fn adapt_mono_stereo() {
        let st = adapt_channels(&[vec![1.0, 0.5]], 2);
        assert_eq!(st.len(), 2);
        let mono = adapt_channels(&[vec![1.0, 0.0], vec![0.0, 1.0]], 1);
        assert_eq!(mono, vec![vec![0.5, 0.5]]);
    }

    #[test]
    fn resample_doubles_length() {
        let out = resample(&[vec![0.0, 1.0, 0.0, -1.0]], 22050, 44100);
        assert_eq!(out[0].len(), 8);
        assert!((out[0][1] - 0.5).abs() < 1e-6);
    }
}
