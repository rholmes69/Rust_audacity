//! Project state: tracks, selection, clipboard and undo history.

use crate::track::{Track, resample};

pub const DEFAULT_RATE: u32 = 44_100;
const MAX_UNDO: usize = 100;

/// A time selection, in samples at the project rate. `start == end` means a cursor.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Selection {
    pub start: usize,
    pub end: usize,
}

impl Selection {
    pub fn cursor(pos: usize) -> Self {
        Self {
            start: pos,
            end: pos,
        }
    }
    pub fn new(a: usize, b: usize) -> Self {
        Self {
            start: a.min(b),
            end: a.max(b),
        }
    }
    pub fn is_empty(&self) -> bool {
        self.start == self.end
    }
    pub fn len(&self) -> usize {
        self.end - self.start
    }
}

#[derive(Clone)]
struct Snapshot {
    tracks: Vec<Track>,
    selection: Selection,
    label: String,
}

/// Audio placed on the clipboard: one entry per copied track (each entry is planar channels).
#[derive(Clone, Default)]
pub struct Clipboard {
    pub clips: Vec<Vec<Vec<f32>>>,
    pub rate: u32,
}

pub struct Project {
    pub tracks: Vec<Track>,
    pub rate: u32,
    pub selection: Selection,
    pub clipboard: Clipboard,
    undo: Vec<Snapshot>,
    redo: Vec<Snapshot>,
    next_id: u64,
    /// Incremented on every change; lets the UI and engine notice edits.
    pub revision: u64,
    pub dirty: bool,
    /// Noise fingerprint captured for Noise Reduction (not part of undo history).
    pub noise_profile: Option<crate::dsp::denoise::NoiseProfile>,
}

impl Default for Project {
    fn default() -> Self {
        Self {
            tracks: Vec::new(),
            rate: DEFAULT_RATE,
            selection: Selection::default(),
            clipboard: Clipboard::default(),
            undo: Vec::new(),
            redo: Vec::new(),
            next_id: 1,
            revision: 0,
            dirty: false,
            noise_profile: None,
        }
    }
}

impl Project {
    pub fn alloc_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Longest track length in samples.
    pub fn len(&self) -> usize {
        self.tracks.iter().map(Track::len).max().unwrap_or(0)
    }

    // ----- undo / redo -------------------------------------------------------

    /// Record the current state before a modification.
    pub fn push_undo(&mut self, label: impl Into<String>) {
        self.undo.push(Snapshot {
            tracks: self.tracks.clone(),
            selection: self.selection,
            label: label.into(),
        });
        if self.undo.len() > MAX_UNDO {
            self.undo.remove(0);
        }
        self.redo.clear();
        self.touch();
    }

    pub fn touch(&mut self) {
        self.revision = self.revision.wrapping_add(1);
        self.dirty = true;
    }

    pub fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }
    pub fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }
    pub fn undo_label(&self) -> Option<&str> {
        self.undo.last().map(|s| s.label.as_str())
    }
    pub fn redo_label(&self) -> Option<&str> {
        self.redo.last().map(|s| s.label.as_str())
    }

    pub fn undo(&mut self) -> bool {
        let Some(snap) = self.undo.pop() else {
            return false;
        };
        let current = Snapshot {
            tracks: std::mem::replace(&mut self.tracks, snap.tracks),
            selection: self.selection,
            label: snap.label,
        };
        self.selection = snap.selection;
        self.redo.push(current);
        self.touch();
        true
    }

    /// Forget the redo stack (used when a failed operation is rolled back).
    pub fn discard_redo(&mut self) {
        self.redo.clear();
    }

    pub fn redo(&mut self) -> bool {
        let Some(snap) = self.redo.pop() else {
            return false;
        };
        let current = Snapshot {
            tracks: std::mem::replace(&mut self.tracks, snap.tracks),
            selection: self.selection,
            label: snap.label,
        };
        self.selection = snap.selection;
        self.undo.push(current);
        self.touch();
        true
    }

    // ----- tracks ------------------------------------------------------------

    pub fn add_track(&mut self, name: impl Into<String>, channels: Vec<Vec<f32>>) -> usize {
        let id = self.alloc_id();
        self.tracks.push(Track::new(id, name, channels));
        self.tracks.len() - 1
    }

    /// Add audio recorded/imported at `rate`, resampling to the project rate.
    /// The first audio added to an empty project sets the project rate instead.
    pub fn add_audio(
        &mut self,
        name: impl Into<String>,
        channels: Vec<Vec<f32>>,
        rate: u32,
    ) -> usize {
        let channels = if self.tracks.is_empty() {
            self.rate = rate;
            channels
        } else {
            resample(&channels, rate, self.rate)
        };
        self.add_track(name, channels)
    }

    /// Indices of the tracks an edit should apply to: selected tracks, or all if none selected.
    pub fn target_tracks(&self) -> Vec<usize> {
        let sel: Vec<usize> = (0..self.tracks.len())
            .filter(|&i| self.tracks[i].selected)
            .collect();
        if sel.is_empty() {
            (0..self.tracks.len()).collect()
        } else {
            sel
        }
    }

    /// The time range an effect should apply to: the selection, or whole project if empty.
    pub fn effect_range(&self) -> (usize, usize) {
        if self.selection.is_empty() {
            (0, self.len())
        } else {
            (self.selection.start, self.selection.end)
        }
    }

    // ----- edit operations ---------------------------------------------------

    pub fn copy(&mut self) -> bool {
        if self.selection.is_empty() {
            return false;
        }
        let Selection { start, end } = self.selection;
        let clips = self
            .target_tracks()
            .into_iter()
            .map(|i| self.tracks[i].extract(start, end))
            .collect();
        self.clipboard = Clipboard {
            clips,
            rate: self.rate,
        };
        true
    }

    pub fn cut(&mut self) -> bool {
        if !self.copy() {
            return false;
        }
        self.push_undo("Cut");
        self.delete_range();
        true
    }

    pub fn delete(&mut self) -> bool {
        if self.selection.is_empty() {
            return false;
        }
        self.push_undo("Delete");
        self.delete_range();
        true
    }

    fn delete_range(&mut self) {
        let Selection { start, end } = self.selection;
        for i in self.target_tracks() {
            self.tracks[i].remove(start, end);
        }
        self.selection = Selection::cursor(start);
    }

    /// Paste the clipboard at the cursor (replacing any selection).
    pub fn paste(&mut self) -> bool {
        if self.clipboard.clips.is_empty() {
            return false;
        }
        self.push_undo("Paste");
        let clips: Vec<Vec<Vec<f32>>> = self
            .clipboard
            .clips
            .iter()
            .map(|c| resample(c, self.clipboard.rate, self.rate))
            .collect();
        let Selection { start, end } = self.selection;
        let mut targets = self.target_tracks();
        if self.tracks.is_empty() {
            targets.clear();
        }
        let mut pasted_len = 0;
        // Paste clip i into target i; extra clips become new tracks.
        for (k, clip) in clips.iter().enumerate() {
            pasted_len = pasted_len.max(clip.first().map_or(0, Vec::len));
            if let Some(&ti) = targets.get(k) {
                self.tracks[ti].replace(start, end, clip);
            } else {
                let mut padded = clip.clone();
                for ch in &mut padded {
                    ch.splice(0..0, std::iter::repeat_n(0.0, start));
                }
                let idx = self.add_track(format!("Pasted {}", self.next_id), padded);
                self.tracks[idx].selected = true;
            }
        }
        self.selection = Selection::new(start, start + pasted_len);
        true
    }

    pub fn silence_selection(&mut self) -> bool {
        if self.selection.is_empty() {
            return false;
        }
        self.push_undo("Silence");
        let Selection { start, end } = self.selection;
        for i in self.target_tracks() {
            self.tracks[i].silence(start, end);
        }
        true
    }

    /// Keep only the selected audio in the target tracks.
    pub fn trim(&mut self) -> bool {
        if self.selection.is_empty() {
            return false;
        }
        self.push_undo("Trim Audio");
        let Selection { start, end } = self.selection;
        for i in self.target_tracks() {
            let t = &mut self.tracks[i];
            let len = t.len();
            t.remove(end.min(len), len);
            t.remove(0, start);
        }
        self.selection = Selection::new(0, end - start);
        true
    }

    /// Copy the selection into a new track (at the same time position).
    pub fn duplicate(&mut self) -> bool {
        if self.selection.is_empty() {
            return false;
        }
        self.push_undo("Duplicate");
        let Selection { start, end } = self.selection;
        for i in self.target_tracks() {
            let mut data = self.tracks[i].extract(start, end);
            for ch in &mut data {
                ch.splice(0..0, std::iter::repeat_n(0.0, start));
            }
            let name = format!("{} (dup)", self.tracks[i].name);
            self.add_track(name, data);
        }
        true
    }

    /// Move the selection into a new track, leaving silence behind.
    pub fn split_new(&mut self) -> bool {
        if self.selection.is_empty() {
            return false;
        }
        self.push_undo("Split New");
        let Selection { start, end } = self.selection;
        for i in self.target_tracks() {
            let mut data = self.tracks[i].extract(start, end);
            self.tracks[i].silence(start, end);
            for ch in &mut data {
                ch.splice(0..0, std::iter::repeat_n(0.0, start));
            }
            let name = format!("{} (split)", self.tracks[i].name);
            self.add_track(name, data);
        }
        true
    }

    pub fn remove_track(&mut self, idx: usize) {
        if idx < self.tracks.len() {
            self.push_undo("Remove Track");
            self.tracks.remove(idx);
        }
    }

    pub fn select_all(&mut self) {
        self.selection = Selection::new(0, self.len());
        for t in &mut self.tracks {
            t.selected = true;
        }
    }

    /// Mix all audible tracks down to stereo (or mono if every track is mono).
    #[allow(clippy::needless_range_loop)]
    pub fn mixdown(&self, start: usize, end: usize, only_targets: bool) -> Vec<Vec<f32>> {
        let any_solo = self.tracks.iter().any(|t| t.solo);
        let indices: Vec<usize> = if only_targets {
            self.target_tracks()
        } else {
            (0..self.tracks.len()).collect()
        };
        let audible: Vec<&Track> = indices
            .into_iter()
            .map(|i| &self.tracks[i])
            .filter(|t| !t.mute && (!any_solo || t.solo))
            .collect();
        let end = end.min(self.len());
        let len = end.saturating_sub(start);
        let stereo = audible.iter().any(|t| t.num_channels() > 1 || t.pan != 0.0);
        let mut out = vec![vec![0.0f32; len]; if stereo { 2 } else { 1 }];
        for t in audible {
            let (gl, gr) = t.lr_gains();
            let tl = t.len();
            for i in 0..len {
                let idx = start + i;
                if idx >= tl {
                    break;
                }
                let l = t.channels[0][idx];
                let r = t.channels.get(1).map_or(l, |c| c[idx]);
                if stereo {
                    out[0][i] += l * gl;
                    out[1][i] += r * gr;
                } else {
                    out[0][i] += l * t.gain;
                }
            }
        }
        out
    }

    /// Replace all target tracks with a single mixed track ("Mix and Render").
    pub fn mix_and_render(&mut self) -> bool {
        if self.tracks.is_empty() {
            return false;
        }
        self.push_undo("Mix and Render");
        let targets = self.target_tracks();
        let len = targets
            .iter()
            .map(|&i| self.tracks[i].len())
            .max()
            .unwrap_or(0);
        let mix = self.mixdown(0, len, true);
        let insert_at = targets[0];
        for &i in targets.iter().rev() {
            self.tracks.remove(i);
        }
        let id = self.alloc_id();
        let mut t = Track::new(id, "Mix", mix);
        t.selected = true;
        self.tracks.insert(insert_at.min(self.tracks.len()), t);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn proj() -> Project {
        let mut p = Project::default();
        p.add_track("a", vec![(0..10).map(|i| i as f32).collect()]);
        p
    }

    #[test]
    fn cut_paste_undo_redo() {
        let mut p = proj();
        p.selection = Selection::new(2, 5);
        assert!(p.cut());
        assert_eq!(p.tracks[0].len(), 7);
        p.selection = Selection::cursor(0);
        assert!(p.paste());
        assert_eq!(&p.tracks[0].channels[0][..3], &[2.0, 3.0, 4.0]);
        assert!(p.undo());
        assert_eq!(p.tracks[0].len(), 7);
        assert!(p.undo());
        assert_eq!(p.tracks[0].len(), 10);
        assert!(p.redo());
        assert_eq!(p.tracks[0].len(), 7);
    }

    #[test]
    fn trim_keeps_selection() {
        let mut p = proj();
        p.selection = Selection::new(3, 6);
        p.trim();
        assert_eq!(*p.tracks[0].channels[0], vec![3.0, 4.0, 5.0]);
    }

    #[test]
    fn mixdown_respects_mute_and_solo() {
        let mut p = Project::default();
        p.add_track("a", vec![vec![1.0; 4]]);
        p.add_track("b", vec![vec![2.0; 4]]);
        assert_eq!(p.mixdown(0, 4, false)[0][0], 3.0);
        p.tracks[0].mute = true;
        assert_eq!(p.mixdown(0, 4, false)[0][0], 2.0);
        p.tracks[0].mute = false;
        p.tracks[0].solo = true;
        assert_eq!(p.mixdown(0, 4, false)[0][0], 1.0);
    }
}
