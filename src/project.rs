//! Project state: tracks, selection, clipboard and undo history.

use std::sync::Arc;

use crate::{
    track::{Track, resample},
    transcript::Transcript,
};

pub const DEFAULT_RATE: u32 = 44_100;
const MAX_UNDO: usize = 100;
/// Samples quieter than this count as silence when time-shifting.
const SILENCE: f32 = 1e-5;

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
    transcript: Option<Arc<Transcript>>,
    labels: Vec<crate::labels::Label>,
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
    /// Word-level transcript of one track (part of undo history).
    pub transcript: Option<Arc<Transcript>>,
    /// Named points / regions on the timeline (part of undo history).
    pub labels: Vec<crate::labels::Label>,
    /// Title, artist, cover art… written into exported files (not part of undo).
    pub metadata: crate::metadata::Metadata,
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
            transcript: None,
            labels: Vec::new(),
            metadata: Default::default(),
        }
    }
}

impl Project {
    /// Make sure future ids are larger than `id`.
    pub fn ensure_ids_above(&mut self, id: u64) {
        self.next_id = self.next_id.max(id + 1);
    }

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
            transcript: self.transcript.clone(),
            labels: self.labels.clone(),
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
            transcript: std::mem::replace(&mut self.transcript, snap.transcript),
            labels: std::mem::replace(&mut self.labels, snap.labels),
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
            transcript: std::mem::replace(&mut self.transcript, snap.transcript),
            labels: std::mem::replace(&mut self.labels, snap.labels),
            label: snap.label,
        };
        self.selection = snap.selection;
        self.undo.push(current);
        self.touch();
        true
    }

    // ----- transcript ----------------------------------------------------------

    /// Index of the track the transcript belongs to (if it still exists).
    pub fn transcript_track(&self) -> Option<usize> {
        let id = self.transcript.as_ref()?.track_id;
        self.tracks.iter().position(|t| t.id == id)
    }

    /// Mutable transcript, if it belongs to track `idx`.
    fn transcript_for(&mut self, idx: usize) -> Option<&mut Transcript> {
        let id = self.tracks.get(idx)?.id;
        let t = self.transcript.as_mut()?;
        if t.track_id != id {
            return None;
        }
        Some(Arc::make_mut(t))
    }

    /// Track `idx` had `[start, old_end)` replaced by `[start, new_end)`.
    pub fn retime_transcript(&mut self, idx: usize, start: usize, old_end: usize, new_end: usize) {
        if let Some(t) = self.transcript_for(idx) {
            t.retime(start, old_end, new_end);
        }
    }

    /// Cut several ranges out of the transcript's track (or every track), last first,
    /// as one undo step. Used by text-based editing ("delete these words").
    pub fn delete_ranges(
        &mut self,
        ranges: &[(usize, usize)],
        all_tracks: bool,
        label: &str,
    ) -> bool {
        let ranges = crate::transcript::merge_ranges(ranges.to_vec());
        let Some(tt) = self.transcript_track() else {
            return false;
        };
        if ranges.is_empty() {
            return false;
        }
        self.push_undo(label);
        let targets: Vec<usize> = if all_tracks {
            (0..self.tracks.len()).collect()
        } else {
            vec![tt]
        };
        let all = self.all_tracks(&targets);
        for &(s, e) in ranges.iter().rev() {
            for &i in &targets {
                let len = self.tracks[i].len();
                if s < len {
                    self.tracks[i].remove(s, e.min(len));
                    self.retime_transcript(i, s, e.min(len), s);
                }
            }
            if all {
                self.retime_labels(s, e, s);
            }
        }
        self.selection = Selection::cursor(ranges[0].0);
        true
    }

    // ----- labels --------------------------------------------------------------

    /// True if `targets` covers every track (then edits move the labels too).
    fn all_tracks(&self, targets: &[usize]) -> bool {
        !self.tracks.is_empty() && targets.len() == self.tracks.len()
    }

    /// Move labels for an edit applied to every track.
    pub fn retime_labels(&mut self, start: usize, old_end: usize, new_end: usize) {
        crate::labels::retime(&mut self.labels, start, old_end, new_end);
    }

    /// Add a label (one undo step). Returns its id.
    pub fn add_label(&mut self, start: usize, end: usize, text: impl Into<String>) -> u64 {
        self.push_undo("Add Label");
        let id = self.alloc_id();
        self.labels.push(crate::labels::Label {
            id,
            start: start.min(end),
            end: start.max(end),
            text: text.into(),
        });
        crate::labels::sort(&mut self.labels);
        id
    }

    /// Chapters for export: one per label, back to back (see `labels::chapter_spans`).
    pub fn chapters(&self) -> Vec<crate::metadata::Chapter> {
        let ms = |s: usize| (s as f64 * 1000.0 / self.rate.max(1) as f64).round() as u32;
        crate::labels::chapter_spans(&self.labels, self.len())
            .into_iter()
            .map(|(s, e, t)| crate::metadata::Chapter {
                start_ms: ms(s),
                end_ms: ms(e),
                title: t,
            })
            .collect()
    }

    // ----- auto-ducking -------------------------------------------------------

    /// Lower `targets` wherever `voice` is speaking by writing their volume envelopes
    /// (replacing any existing envelope). Uses the transcript's words when it belongs to
    /// the voice track and `use_words` is set, otherwise the voice level.
    /// Returns the number of speech regions found.
    pub fn auto_duck(
        &mut self,
        voice: usize,
        targets: &[usize],
        params: &crate::dsp::duck::DuckParams,
        use_words: bool,
    ) -> Result<usize, String> {
        use crate::dsp::duck;
        let v = self.tracks.get(voice).ok_or("no such voice track")?;
        let targets: Vec<usize> = targets
            .iter()
            .copied()
            .filter(|&t| t != voice && t < self.tracks.len())
            .collect();
        if targets.is_empty() {
            return Err("Choose at least one track to duck (other than the voice).".into());
        }
        let regions = match (&self.transcript, use_words) {
            (Some(tr), true) if tr.track_id == v.id => {
                let words: Vec<(usize, usize)> =
                    tr.words.iter().map(|w| (w.start, w.end)).collect();
                duck::regions_from_words(&words, self.rate, params.hold_ms)
            }
            _ => {
                let data: Vec<Vec<f32>> = v.channels.iter().map(|c| c.to_vec()).collect();
                let mono = crate::track::adapt_channels(&data, 1)
                    .pop()
                    .unwrap_or_default();
                // Track gain counts: a quiet voice track at +6 dB is louder than it looks.
                let mono: Vec<f32> = mono.iter().map(|x| x * v.gain).collect();
                duck::detect_speech(&mono, self.rate, params.threshold_db, params.hold_ms)
            }
        };
        if regions.is_empty() {
            return Err(
                "No speech was found on the voice track. Try a lower threshold (e.g. −45 dB)."
                    .into(),
            );
        }
        self.push_undo("Auto Duck");
        for t in targets {
            let len = self.tracks[t].len();
            self.tracks[t].envelope = duck::duck_envelope(&regions, len, self.rate, params);
        }
        Ok(regions.len())
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
        let targets = self.target_tracks();
        if self.all_tracks(&targets) {
            self.retime_labels(start, end, start);
        }
        for i in targets {
            let e = end.min(self.tracks[i].len());
            self.tracks[i].remove(start, end);
            if e > start {
                self.retime_transcript(i, start, e, start);
            }
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
                let len = self.tracks[ti].len();
                let clip_len = clip.first().map_or(0, Vec::len);
                self.tracks[ti].replace(start, end, clip);
                let s = start.min(len);
                self.retime_transcript(ti, s, end.min(len), s + clip_len);
            } else {
                let mut padded = clip.clone();
                for ch in &mut padded {
                    ch.splice(0..0, std::iter::repeat_n(0.0, start));
                }
                let idx = self.add_track(format!("Pasted {}", self.next_id), padded);
                self.tracks[idx].selected = true;
            }
        }
        if self.all_tracks(&targets) && clips.len() >= targets.len() {
            self.retime_labels(start, end, start + pasted_len);
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
        let targets = self.target_tracks();
        if self.all_tracks(&targets) {
            let len = self.len();
            self.retime_labels(end.min(len), len, end.min(len));
            self.retime_labels(0, start.min(len), 0);
        }
        for i in targets {
            let t = &mut self.tracks[i];
            let len = t.len();
            t.remove(end.min(len), len);
            t.remove(0, start);
            if let Some(tr) = self.transcript_for(i) {
                tr.retime(end.min(len), len, end.min(len));
                tr.retime(0, start.min(len), 0);
            }
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

    // ----- time shifting -----------------------------------------------------

    /// How far track `idx` can move left: the length of its leading silence.
    pub fn leading_silence(&self, idx: usize) -> usize {
        let t = &self.tracks[idx];
        let len = t.len();
        (0..len)
            .find(|&i| t.channels.iter().any(|c| c[i].abs() > SILENCE))
            .unwrap_or(len)
    }

    /// Clamp a proposed time shift of track `idx` so audio never moves before 0.
    pub fn clamp_track_shift(&self, idx: usize, delta: i64) -> i64 {
        delta.max(-(self.leading_silence(idx) as i64))
    }

    /// Move a whole track along the timeline by `delta` samples (Time Shift tool).
    /// Moving right inserts silence at the start; moving left removes leading silence
    /// (and never audio). Returns the shift that was actually applied.
    pub fn shift_track(&mut self, idx: usize, delta: i64) -> i64 {
        if idx >= self.tracks.len() {
            return 0;
        }
        let delta = self.clamp_track_shift(idx, delta);
        if delta == 0 {
            return 0;
        }
        self.push_undo("Time Shift");
        let t = &mut self.tracks[idx];
        if delta > 0 {
            let n = t.num_channels();
            t.insert(0, &vec![vec![0.0; delta as usize]; n]);
        } else {
            t.remove(0, (-delta) as usize);
        }
        let selected = t.selected;
        if let Some(tr) = self.transcript_for(idx) {
            tr.shift(delta);
        }
        if selected && !self.selection.is_empty() {
            let s = (self.selection.start as i64 + delta).max(0) as usize;
            self.selection = Selection::new(s, s + self.selection.len());
        }
        delta
    }

    /// Move the selected audio (the "clip") of the target tracks by `delta` samples,
    /// leaving silence behind and overwriting whatever is at the destination.
    /// If `dest_track` is given and exactly one track is being moved, the audio moves
    /// onto that track instead. Returns false if there is nothing to move.
    pub fn move_selection(&mut self, delta: i64, dest_track: Option<usize>) -> bool {
        if self.selection.is_empty() || self.tracks.is_empty() {
            return false;
        }
        let Selection { start, end } = self.selection;
        let targets: Vec<usize> = self
            .target_tracks()
            .into_iter()
            .filter(|&i| self.tracks[i].len() > start)
            .collect();
        if targets.is_empty() {
            return false;
        }
        let dest_track = dest_track.filter(|&d| targets.len() == 1 && d < self.tracks.len());
        if delta == 0 && dest_track.is_none_or(|d| d == targets[0]) {
            return false;
        }
        let new_start = (start as i64 + delta).max(0) as usize;
        let len = end - start;
        self.push_undo("Move Clip");
        let clips: Vec<(usize, Vec<Vec<f32>>)> = targets
            .iter()
            .map(|&i| (i, self.tracks[i].extract(start, end)))
            .collect();
        for &(i, _) in &clips {
            self.tracks[i].silence(start, end);
        }
        // Words travel with their audio; words under the destination are overwritten.
        if let Some(tt) = self.transcript_track()
            && let Some(tr) = self.transcript.as_mut().map(Arc::make_mut)
        {
            let new_track = |i: usize| dest_track.unwrap_or(i);
            let moves_words = clips.iter().any(|&(i, _)| i == tt);
            let mut moved: Vec<crate::transcript::Word> = Vec::new();
            if moves_words {
                let inside = tr.words_in(start, end);
                for &k in inside.iter().rev() {
                    moved.push(tr.words.remove(k));
                }
            }
            if clips.iter().any(|&(i, _)| new_track(i) == tt) {
                tr.drop_range(new_start, new_start + len);
            }
            if moves_words && new_track(tt) == tt {
                for mut w in moved {
                    w.start = (w.start as i64 + new_start as i64 - start as i64).max(0) as usize;
                    w.end = (w.end as i64 + new_start as i64 - start as i64).max(0) as usize;
                    tr.words.push(w);
                }
                tr.words.sort_by_key(|w| w.start);
            }
        }
        for (i, data) in clips {
            let ti = dest_track.unwrap_or(i);
            let t = &mut self.tracks[ti];
            let data = crate::track::adapt_channels(&data, t.num_channels());
            t.pad_to(new_start + len);
            let mut ch = 0;
            t.process(new_start, new_start + len, |buf| {
                let src = &data[ch.min(data.len() - 1)];
                buf[..src.len().min(len)].copy_from_slice(&src[..src.len().min(len)]);
                ch += 1;
            });
        }
        if let Some(d) = dest_track {
            for (j, t) in self.tracks.iter_mut().enumerate() {
                t.selected = j == d;
            }
        }
        self.selection = Selection::new(new_start, new_start + len);
        true
    }

    pub fn remove_track(&mut self, idx: usize) {
        if idx < self.tracks.len() {
            self.push_undo("Remove Track");
            self.tracks.remove(idx);
            self.drop_orphan_transcript();
        }
    }

    /// Forget the transcript if its track no longer exists.
    pub fn drop_orphan_transcript(&mut self) {
        if self.transcript.is_some() && self.transcript_track().is_none() {
            self.transcript = None;
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
            let env = t.envelope.gains(start, len.min(tl.saturating_sub(start)));
            for i in 0..len {
                let idx = start + i;
                if idx >= tl {
                    break;
                }
                let g = env.as_ref().map_or(1.0, |e| e[i]);
                let l = t.channels[0][idx] * g;
                let r = t.channels.get(1).map_or(l, |c| c[idx] * g);
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
        self.drop_orphan_transcript();
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
    fn shift_track_never_cuts_audio() {
        let mut p = Project::default();
        p.add_track("a", vec![vec![0.0, 0.0, 0.0, 1.0, 2.0]]);
        assert_eq!(p.shift_track(0, 2), 2);
        assert_eq!(
            *p.tracks[0].channels[0],
            [0.0; 5]
                .iter()
                .chain(&[1.0, 2.0])
                .copied()
                .collect::<Vec<_>>()
        );
        // Left by more than the leading silence is clamped.
        assert_eq!(p.shift_track(0, -100), -5);
        assert_eq!(*p.tracks[0].channels[0], vec![1.0, 2.0]);
        assert_eq!(p.shift_track(0, -1), 0);
        assert!(p.undo());
        assert_eq!(p.tracks[0].len(), 7);
    }

    #[test]
    fn move_selection_within_and_between_tracks() {
        let mut p = Project::default();
        p.add_track("a", vec![(1..=6).map(|i| i as f32).collect()]);
        p.add_track("b", vec![vec![0.0; 2], vec![0.0; 2]]);
        p.tracks[1].selected = false;
        p.selection = Selection::new(0, 2);
        // Move [1,2] right by 3 on the same track.
        assert!(p.move_selection(3, None));
        assert_eq!(*p.tracks[0].channels[0], vec![0.0, 0.0, 3.0, 1.0, 2.0, 6.0]);
        assert_eq!(p.selection, Selection::new(3, 5));
        // Move it onto the stereo track b, 1 sample earlier, past b's end.
        assert!(p.move_selection(-1, Some(1)));
        assert_eq!(*p.tracks[0].channels[0], vec![0.0, 0.0, 3.0, 0.0, 0.0, 6.0]);
        assert_eq!(*p.tracks[1].channels[0], vec![0.0, 0.0, 1.0, 2.0]);
        assert_eq!(*p.tracks[1].channels[1], vec![0.0, 0.0, 1.0, 2.0]);
        assert!(p.tracks[1].selected && !p.tracks[0].selected);
        assert!(p.undo() && p.undo());
        assert_eq!(*p.tracks[0].channels[0], vec![1.0, 2.0, 3.0, 4.0, 5.0, 6.0]);
    }

    #[test]
    fn transcript_follows_edits_and_undo() {
        use crate::transcript::Word;
        let mut p = Project::default();
        p.add_track("voice", vec![(0..100).map(|i| i as f32).collect()]);
        let w = |s, e, t: &str| Word {
            start: s,
            end: e,
            text: t.into(),
            prob: 1.0,
        };
        p.transcript = Some(Arc::new(Transcript {
            track_id: p.tracks[0].id,
            language: "en".into(),
            words: vec![w(10, 20, "one"), w(30, 40, "um"), w(50, 60, "three")],
        }));
        // Text-based edit: delete "um".
        assert!(p.delete_ranges(&[(30, 40)], true, "Delete words"));
        assert_eq!(p.tracks[0].len(), 90);
        assert_eq!(p.tracks[0].channels[0][30], 40.0);
        let words = &p.transcript.as_ref().unwrap().words;
        assert_eq!(words.len(), 2);
        assert_eq!((words[1].start, words[1].end), (40, 50));
        // Ordinary delete before the words shifts them too.
        p.selection = Selection::new(0, 5);
        p.delete();
        assert_eq!(p.transcript.as_ref().unwrap().words[0].start, 5);
        // Undo restores both audio and words.
        assert!(p.undo() && p.undo());
        assert_eq!(p.transcript.as_ref().unwrap().words.len(), 3);
        assert_eq!(p.tracks[0].len(), 100);
        // Removing the track drops the transcript.
        p.remove_track(0);
        assert!(p.transcript.is_none());
    }

    #[test]
    fn auto_duck_writes_envelopes_used_by_mixdown() {
        let rate = 1000;
        let mut p = Project {
            rate,
            ..Project::default()
        };
        let mut voice = vec![0.0f32; 6000];
        voice[2000..3000].iter_mut().for_each(|x| *x = 0.5);
        p.add_track("voice", vec![voice]);
        p.add_track("music", vec![vec![0.5; 6000]]);
        let params = crate::dsp::duck::DuckParams::default();
        assert_eq!(p.auto_duck(0, &[1], &params, false), Ok(1));
        assert!(p.tracks[0].envelope.is_empty());
        let mix = p.mixdown(0, 6000, false);
        assert!(
            (mix[0][500] - 0.5).abs() < 1e-6,
            "music at full level before speech"
        );
        let ducked = 0.5 * 10f32.powf(-12.0 / 20.0);
        assert!(
            (mix[0][2500] - 0.5 - ducked).abs() < 1e-4,
            "voice + ducked music"
        );
        assert!(p.undo());
        assert!(p.tracks[1].envelope.is_empty());
        assert!(p.auto_duck(0, &[0], &params, false).is_err());
    }

    #[test]
    fn labels_follow_edits_to_all_tracks_and_undo() {
        let mut p = Project {
            rate: 1000,
            ..Default::default()
        };
        p.add_track("a", vec![vec![0.1; 10_000]]);
        p.add_track("b", vec![vec![0.2; 10_000]]);
        p.add_label(1000, 1000, "One");
        p.add_label(5000, 6000, "Two");
        // Deleting on every track moves later labels.
        p.select_all();
        p.selection = Selection::new(2000, 3000);
        assert!(p.delete());
        assert_eq!((p.labels[1].start, p.labels[1].end), (4000, 5000));
        // Deleting on one track only leaves labels alone.
        p.tracks[1].selected = false;
        p.selection = Selection::new(0, 500);
        assert!(p.delete());
        assert_eq!(p.labels[0].start, 1000);
        let ch = p.chapters();
        assert_eq!(ch.len(), 2);
        assert_eq!((ch[0].start_ms, ch[0].title.as_str()), (1000, "One"));
        assert!(p.undo() && p.undo());
        assert_eq!(p.labels[1].start, 5000);
        assert!(p.undo() && p.undo());
        assert!(p.labels.is_empty());
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
