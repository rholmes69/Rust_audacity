//! Labels: named points or regions on the timeline (like Audacity's label track).
//!
//! Labels belong to the project, not to a track. They move with edits that apply to every
//! track (so they stay on the same audio), and are used for chapters and for exporting
//! each region as its own file.

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Label {
    pub id: u64,
    /// Start / end in samples. `start == end` is a point label.
    pub start: usize,
    pub end: usize,
    pub text: String,
}

impl Label {
    pub fn is_point(&self) -> bool {
        self.start == self.end
    }
}

/// Keep labels in step with an edit (to all tracks) that turned `[start, old_end)` into
/// `[start, new_end)`. Labels entirely inside a deleted range are removed.
pub fn retime(labels: &mut Vec<Label>, start: usize, old_end: usize, new_end: usize) {
    if old_end == new_end {
        return;
    }
    let deleting = new_end == start;
    let old_len = (old_end - start) as f64;
    let new_len = (new_end - start) as f64;
    let map = |p: usize| -> usize {
        if p < start {
            p
        } else if p >= old_end {
            (p as i64 + new_end as i64 - old_end as i64).max(start as i64) as usize
        } else if deleting {
            start
        } else {
            start + ((p - start) as f64 * new_len / old_len.max(1.0)).round() as usize
        }
    };
    if deleting {
        // A point inside the deleted range goes away, as does a region entirely inside it.
        labels.retain(|l| {
            let inside = l.start >= start
                && if l.is_point() {
                    l.start < old_end
                } else {
                    l.end <= old_end
                };
            !inside
        });
    }
    for l in labels.iter_mut() {
        l.start = map(l.start);
        l.end = map(l.end).max(l.start);
    }
    labels.sort_by_key(|l| (l.start, l.end));
}

/// Keep labels in time order.
pub fn sort(labels: &mut [Label]) {
    labels.sort_by_key(|l| (l.start, l.end));
}

/// The pieces "Export Multiple" writes: region labels as they are; a point label runs to
/// the next label of any kind (or the end). `before_first` adds the audio before the first
/// label as a piece called "Start".
pub fn export_regions(
    labels: &[Label],
    len: usize,
    before_first: bool,
) -> Vec<(usize, usize, String)> {
    let mut sorted: Vec<&Label> = labels.iter().filter(|l| l.start < len).collect();
    sorted.sort_by_key(|l| (l.start, l.end));
    let mut out = Vec::new();
    if before_first
        && let Some(first) = sorted.first()
        && first.start > 0
    {
        out.push((0, first.start, "Start".to_string()));
    }
    for (i, l) in sorted.iter().enumerate() {
        let end = if l.is_point() {
            sorted[i + 1..]
                .iter()
                .map(|n| n.start)
                .find(|&s| s > l.start)
                .unwrap_or(len)
        } else {
            l.end.min(len)
        };
        if end > l.start {
            out.push((l.start, end, l.text.clone()));
        }
    }
    out
}

/// Chapters: one per label, each running until the next label starts (or the end), so
/// they never overlap. Returns (start, end, title).
pub fn chapter_spans(labels: &[Label], len: usize) -> Vec<(usize, usize, String)> {
    let mut sorted: Vec<&Label> = labels.iter().filter(|l| l.start < len).collect();
    sorted.sort_by_key(|l| (l.start, l.end));
    sorted.dedup_by_key(|l| l.start);
    (0..sorted.len())
        .map(|i| {
            let end = sorted.get(i + 1).map_or(len, |n| n.start);
            (sorted[i].start, end, sorted[i].text.clone())
        })
        .collect()
}

fn fmt_secs(samples: usize, rate: u32) -> String {
    format!("{:.6}", samples as f64 / rate.max(1) as f64)
}

/// Audacity label file: `start<TAB>end<TAB>text` per line, times in seconds.
pub fn to_audacity_txt(labels: &[Label], rate: u32) -> String {
    labels
        .iter()
        .map(|l| {
            format!(
                "{}\t{}\t{}\n",
                fmt_secs(l.start, rate),
                fmt_secs(l.end, rate),
                l.text
            )
        })
        .collect()
}

/// Read an Audacity label file. Spectral-selection lines (starting with `\`) are skipped.
pub fn parse_audacity_txt(text: &str, rate: u32) -> Result<Vec<(usize, usize, String)>, String> {
    let mut out = Vec::new();
    for (n, line) in text.lines().enumerate() {
        let line = line.trim_end_matches('\r');
        if line.trim().is_empty() || line.starts_with('\\') {
            continue;
        }
        let mut parts = line.splitn(3, '\t');
        let parse = |s: Option<&str>| -> Result<f64, String> {
            s.unwrap_or("")
                .trim()
                .replace(',', ".")
                .parse::<f64>()
                .map_err(|_| format!("line {}: expected “start<TAB>end<TAB>text”", n + 1))
        };
        let a = parse(parts.next())?;
        let b = parse(parts.next())?;
        let text = parts.next().unwrap_or("").to_string();
        let to = |t: f64| (t.max(0.0) * rate as f64).round() as usize;
        out.push((to(a.min(b)), to(a.max(b)), text));
    }
    Ok(out)
}

/// Make a label text safe as a file name.
pub fn file_name_part(text: &str) -> String {
    let cleaned: String = text
        .chars()
        .map(|c| {
            if "\\/:*?\"<>|".contains(c) || c.is_control() {
                '_'
            } else {
                c
            }
        })
        .collect();
    let t = cleaned.trim().trim_end_matches('.').to_string();
    if t.is_empty() {
        "Untitled".into()
    } else {
        t.chars().take(80).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn l(id: u64, s: usize, e: usize, t: &str) -> Label {
        Label {
            id,
            start: s,
            end: e,
            text: t.into(),
        }
    }

    #[test]
    fn retime_moves_and_removes() {
        let mut v = vec![
            l(1, 10, 10, "a"),
            l(2, 30, 50, "b"),
            l(3, 60, 60, "c"),
            l(4, 100, 120, "d"),
        ];
        // Delete [25, 65): "c" (point inside) goes; "b" (region inside) goes; "d" moves left 40.
        retime(&mut v, 25, 65, 25);
        assert_eq!(
            v.iter().map(|x| x.text.as_str()).collect::<Vec<_>>(),
            vec!["a", "d"]
        );
        assert_eq!((v[1].start, v[1].end), (60, 80));
        // Insert 5 at 0.
        retime(&mut v, 0, 0, 5);
        assert_eq!(v[0].start, 15);
        // A region partly cut keeps its outside part.
        let mut w = vec![l(1, 10, 40, "x")];
        retime(&mut w, 30, 60, 30);
        assert_eq!((w[0].start, w[0].end), (10, 30));
    }

    #[test]
    fn regions_and_txt() {
        let v = vec![
            l(1, 100, 100, "Song A"),
            l(2, 400, 400, "Song B"),
            l(3, 700, 800, "Bonus"),
        ];
        let r = export_regions(&v, 1000, true);
        assert_eq!(
            r,
            vec![
                (0, 100, "Start".into()),
                (100, 400, "Song A".into()),
                (400, 700, "Song B".into()),
                (700, 800, "Bonus".into())
            ]
        );
        assert_eq!(
            chapter_spans(&v, 1000),
            vec![
                (100, 400, "Song A".into()),
                (400, 700, "Song B".into()),
                (700, 1000, "Bonus".into())
            ]
        );
        let txt = to_audacity_txt(&v, 100);
        assert!(txt.starts_with("1.000000\t1.000000\tSong A\n"));
        let back = parse_audacity_txt(&(txt + "\\\t100\t200\n"), 100).unwrap();
        assert_eq!(back[2], (700, 800, "Bonus".into()));
        assert_eq!(back.len(), 3);
        assert!(parse_audacity_txt("oops", 100).is_err());
        assert_eq!(file_name_part("AC/DC: Live?"), "AC_DC_ Live_");
    }
}
