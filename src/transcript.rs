//! Word-level transcripts attached to a track.
//!
//! Word times are stored in samples at the project rate so they can be kept in sync
//! with edits: every operation that removes, inserts or stretches audio on the
//! transcript's track calls [`Transcript::retime`].

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Word {
    /// Start / end in samples (project rate).
    pub start: usize,
    pub end: usize,
    /// The word as recognised, including any punctuation (no surrounding spaces).
    pub text: String,
    /// Recognition confidence 0–1 (1 if unknown).
    #[serde(default = "one")]
    pub prob: f32,
}

fn one() -> f32 {
    1.0
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Transcript {
    /// Id of the track the words belong to.
    pub track_id: u64,
    /// Language code reported by the recogniser (e.g. "en").
    pub language: String,
    pub words: Vec<Word>,
}

/// Words treated as filler by "Remove filler words".
pub const FILLERS: [&str; 12] = [
    "um", "umm", "uh", "uhh", "uhm", "erm", "er", "ah", "hmm", "mm", "mhm", "eh",
];

impl Transcript {
    /// Keep word times in step with an edit that turned `[start, old_end)` into
    /// `[start, new_end)`: later words shift, words inside are scaled (or dropped when
    /// the range was deleted).
    pub fn retime(&mut self, start: usize, old_end: usize, new_end: usize) {
        if old_end == new_end {
            return;
        }
        let old_len = old_end.saturating_sub(start) as f64;
        let new_len = new_end.saturating_sub(start) as f64;
        let map = |p: usize| -> usize {
            if p < start {
                p
            } else if p >= old_end {
                (p as i64 + new_end as i64 - old_end as i64).max(start as i64) as usize
            } else {
                start + ((p - start) as f64 * new_len / old_len.max(1.0)).round() as usize
            }
        };
        for w in &mut self.words {
            w.start = map(w.start);
            w.end = map(w.end);
        }
        // Words that were entirely inside a deleted range collapse to nothing.
        self.words.retain(|w| w.end > w.start);
    }

    /// Shift every word by `delta` samples (whole-track time shift).
    pub fn shift(&mut self, delta: i64) {
        for w in &mut self.words {
            w.start = (w.start as i64 + delta).max(0) as usize;
            w.end = (w.end as i64 + delta).max(0) as usize;
        }
        self.words.retain(|w| w.end > w.start);
    }

    /// Drop words overlapping `[start, end)` by more than half their length.
    pub fn drop_range(&mut self, start: usize, end: usize) {
        self.words.retain(|w| {
            let ov = w.end.min(end).saturating_sub(w.start.max(start));
            ov * 2 <= w.end - w.start
        });
    }

    /// Indices of words that mostly fall inside `[start, end)`.
    pub fn words_in(&self, start: usize, end: usize) -> Vec<usize> {
        (0..self.words.len())
            .filter(|&i| {
                let w = &self.words[i];
                let ov = w.end.min(end).saturating_sub(w.start.max(start));
                ov * 2 > w.end - w.start
            })
            .collect()
    }

    /// Index of the word playing at sample `pos` (or the last word before it).
    pub fn word_at(&self, pos: usize) -> Option<usize> {
        let i = self.words.partition_point(|w| w.start <= pos);
        i.checked_sub(1)
            .filter(|&j| self.words[j].end + 4_410 >= pos)
    }

    /// Sample ranges covering filler words ("um", "uh", …), merged where adjacent.
    pub fn filler_ranges(&self) -> Vec<(usize, usize)> {
        let ranges: Vec<(usize, usize)> = self
            .words
            .iter()
            .filter(|w| is_filler(&w.text))
            .map(|w| (w.start, w.end))
            .collect();
        merge_ranges(ranges)
    }

    /// Plain text, one paragraph per pause longer than `para_gap` samples.
    pub fn to_text(&self, para_gap: usize) -> String {
        let mut out = String::new();
        let mut prev_end: Option<usize> = None;
        for w in &self.words {
            match prev_end {
                Some(e) if w.start.saturating_sub(e) > para_gap => out.push_str("\n\n"),
                Some(_) => out.push(' '),
                None => {}
            }
            out.push_str(&w.text);
            prev_end = Some(w.end);
        }
        out.push('\n');
        out
    }

    /// SubRip subtitles: lines of up to ~`max_chars` characters / 6 seconds, broken at
    /// pauses and sentence ends.
    pub fn to_srt(&self, rate: u32, max_chars: usize) -> String {
        let rate = rate.max(1) as f64;
        let max_len = (6.0 * rate) as usize;
        let mut cues: Vec<(usize, usize, String)> = Vec::new();
        for w in &self.words {
            let start_new = match cues.last() {
                None => true,
                Some((s, e, text)) => {
                    text.len() + 1 + w.text.len() > max_chars
                        || w.end.saturating_sub(*s) > max_len
                        || w.start.saturating_sub(*e) > (0.8 * rate) as usize
                        || text.ends_with(['.', '?', '!'])
                }
            };
            if start_new {
                cues.push((w.start, w.end, w.text.clone()));
            } else if let Some((_, e, text)) = cues.last_mut() {
                *e = w.end;
                text.push(' ');
                text.push_str(&w.text);
            }
        }
        let ts = |s: usize| {
            let ms = (s as f64 / rate * 1000.0).round() as u64;
            format!(
                "{:02}:{:02}:{:02},{:03}",
                ms / 3_600_000,
                ms / 60_000 % 60,
                ms / 1000 % 60,
                ms % 1000
            )
        };
        let mut out = String::new();
        for (i, (s, e, text)) in cues.iter().enumerate() {
            out.push_str(&format!(
                "{}\n{} --> {}\n{}\n\n",
                i + 1,
                ts(*s),
                ts(*e),
                text
            ));
        }
        out
    }
}

/// True for "um", "Uh,", "hmm." etc.
pub fn is_filler(text: &str) -> bool {
    let t: String = text
        .chars()
        .filter(|c| c.is_alphabetic())
        .collect::<String>()
        .to_lowercase();
    FILLERS.contains(&t.as_str())
}

/// Sort and merge overlapping or touching ranges.
pub fn merge_ranges(mut r: Vec<(usize, usize)>) -> Vec<(usize, usize)> {
    r.sort_unstable();
    let mut out: Vec<(usize, usize)> = Vec::with_capacity(r.len());
    for (s, e) in r {
        match out.last_mut() {
            Some(last) if s <= last.1 => last.1 = last.1.max(e),
            _ => out.push((s, e)),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tr() -> Transcript {
        let w = |s, e, t: &str| Word {
            start: s,
            end: e,
            text: t.into(),
            prob: 1.0,
        };
        Transcript {
            track_id: 1,
            language: "en".into(),
            words: vec![
                w(100, 200, "Hello"),
                w(250, 300, "um,"),
                w(320, 400, "world."),
                w(1000, 1100, "Again"),
            ],
        }
    }

    #[test]
    fn retime_delete_insert_stretch() {
        let mut t = tr();
        // Delete the "um" (250..300): later words move 50 earlier, "um" disappears.
        t.retime(250, 300, 250);
        assert_eq!(t.words.len(), 3);
        assert_eq!((t.words[1].start, t.words[1].end), (270, 350));
        // Insert 100 samples at 0: everything moves right.
        t.retime(0, 0, 100);
        assert_eq!(t.words[0].start, 200);
        // Stretch 200..500 to twice its length.
        t.retime(200, 500, 800);
        assert_eq!((t.words[0].start, t.words[0].end), (200, 400));
        assert_eq!(t.words[2].start, 1050 + 300);
    }

    #[test]
    fn fillers_text_and_srt() {
        let t = tr();
        assert_eq!(t.filler_ranges(), vec![(250, 300)]);
        assert!(is_filler("Um,") && is_filler("hmm.") && !is_filler("umbrella"));
        assert_eq!(t.to_text(500), "Hello um, world.\n\nAgain\n");
        let srt = t.to_srt(100, 42);
        assert!(
            srt.starts_with(
                "1\n00:00:01,000 --> 00:00:04,000\nHello um, world.\n\n2\n00:00:10,000"
            )
        );
        assert_eq!(t.words_in(240, 410), vec![1, 2]);
        assert_eq!(t.word_at(330), Some(2));
        assert_eq!(t.word_at(50), None);
    }

    #[test]
    fn merging() {
        assert_eq!(
            merge_ranges(vec![(5, 8), (0, 2), (2, 3), (7, 9)]),
            vec![(0, 3), (5, 9)]
        );
    }
}
