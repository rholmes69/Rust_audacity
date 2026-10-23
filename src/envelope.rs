//! Per-track volume envelopes ("automation"): gain points in dB, interpolated linearly
//! in dB between points. An empty envelope means a flat 0 dB.
//!
//! Points are stored in samples and moved along with every edit to the track (see
//! `Track::remove` / `insert` / `replace`), so they stay attached to the audio.

use serde::{Deserialize, Serialize};

/// Gains at or below this are treated as silence.
pub const MIN_DB: f32 = -60.0;
pub const MAX_DB: f32 = 12.0;

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct EnvPoint {
    pub pos: usize,
    pub db: f32,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Envelope {
    /// Sorted by position.
    pub points: Vec<EnvPoint>,
}

pub fn db_to_gain(db: f32) -> f32 {
    if db <= MIN_DB {
        0.0
    } else {
        10f32.powf(db / 20.0)
    }
}

impl Envelope {
    pub fn from_points(mut points: Vec<EnvPoint>) -> Self {
        for p in &mut points {
            p.db = p.db.clamp(MIN_DB, MAX_DB);
        }
        points.sort_by_key(|p| p.pos);
        points.dedup_by(|b, a| a.pos == b.pos && a.db == b.db);
        Self { points }
    }

    pub fn is_empty(&self) -> bool {
        self.points.is_empty()
    }

    /// Level in dB at a (fractional) sample position.
    pub fn db_at(&self, pos: f64) -> f32 {
        let pts = &self.points;
        match pts.len() {
            0 => 0.0,
            1 => pts[0].db,
            _ => {
                let i = pts.partition_point(|p| (p.pos as f64) <= pos);
                if i == 0 {
                    pts[0].db
                } else if i == pts.len() {
                    pts[i - 1].db
                } else {
                    let (a, b) = (pts[i - 1], pts[i]);
                    let t = ((pos - a.pos as f64) / (b.pos - a.pos).max(1) as f64) as f32;
                    a.db + (b.db - a.db) * t
                }
            }
        }
    }

    pub fn gain_at(&self, pos: f64) -> f32 {
        if self.points.is_empty() {
            1.0
        } else {
            db_to_gain(self.db_at(pos))
        }
    }

    /// Per-sample gains for `[start, start + len)`, or `None` when flat at 0 dB.
    pub fn gains(&self, start: usize, len: usize) -> Option<Vec<f32>> {
        if self.points.is_empty() {
            return None;
        }
        let mut out = Vec::with_capacity(len);
        let pts = &self.points;
        let mut i = pts.partition_point(|p| p.pos <= start);
        for k in 0..len {
            let pos = start + k;
            while i < pts.len() && pts[i].pos <= pos {
                i += 1;
            }
            let db = if i == 0 {
                pts[0].db
            } else if i == pts.len() {
                pts[i - 1].db
            } else {
                let (a, b) = (pts[i - 1], pts[i]);
                a.db + (b.db - a.db) * ((pos - a.pos) as f32 / (b.pos - a.pos).max(1) as f32)
            };
            out.push(db_to_gain(db));
        }
        Some(out)
    }

    /// Add a point; returns its index.
    pub fn add(&mut self, pos: usize, db: f32) -> usize {
        let i = self.points.partition_point(|p| p.pos <= pos);
        self.points.insert(
            i,
            EnvPoint {
                pos,
                db: db.clamp(MIN_DB, MAX_DB),
            },
        );
        i
    }

    /// Move point `i`, keeping the list sorted. Returns its new index.
    pub fn set(&mut self, i: usize, pos: usize, db: f32) -> usize {
        if i >= self.points.len() {
            return i;
        }
        self.points.remove(i);
        self.add(pos, db)
    }

    /// Keep points in step with an edit that turned `[start, old_end)` into
    /// `[start, new_end)`. Deleting a range keeps the curve's value on both sides.
    pub fn retime(&mut self, start: usize, old_end: usize, new_end: usize) {
        if old_end == new_end || self.points.is_empty() {
            return;
        }
        if new_end == start && old_end > start {
            // Deletion: remember the level either side of the cut.
            let before = self.db_at(start as f64);
            let after = self.db_at(old_end as f64);
            let had_inside = self
                .points
                .iter()
                .any(|p| p.pos >= start && p.pos < old_end);
            let removed = old_end - start;
            self.points.retain(|p| p.pos < start || p.pos >= old_end);
            for p in &mut self.points {
                if p.pos >= old_end {
                    p.pos -= removed;
                }
            }
            if had_inside || (before - after).abs() > 0.01 {
                let has_left = self.points.iter().any(|p| p.pos < start);
                let has_right = self.points.iter().any(|p| p.pos >= start);
                if has_left && start > 0 {
                    self.add(start - 1, before);
                }
                if has_right {
                    self.add(start, after);
                }
            }
            return;
        }
        let (old_len, new_len) = ((old_end - start) as f64, (new_end - start) as f64);
        for p in &mut self.points {
            if p.pos >= old_end {
                p.pos = (p.pos as i64 + new_end as i64 - old_end as i64) as usize;
            } else if p.pos >= start {
                p.pos =
                    start + ((p.pos - start) as f64 * new_len / old_len.max(1.0)).round() as usize;
            }
        }
        self.points.sort_by_key(|p| p.pos);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env() -> Envelope {
        Envelope::from_points(vec![
            EnvPoint { pos: 100, db: 0.0 },
            EnvPoint {
                pos: 200,
                db: -20.0,
            },
            EnvPoint {
                pos: 300,
                db: -20.0,
            },
            EnvPoint { pos: 400, db: 0.0 },
        ])
    }

    #[test]
    fn interpolation_and_gains() {
        let e = env();
        assert_eq!(e.db_at(0.0), 0.0);
        assert!((e.db_at(150.0) + 10.0).abs() < 1e-4);
        assert_eq!(e.db_at(250.0), -20.0);
        assert_eq!(e.db_at(1000.0), 0.0);
        let g = e.gains(140, 120).unwrap();
        for (k, gk) in g.iter().enumerate() {
            assert!((gk - e.gain_at((140 + k) as f64)).abs() < 1e-5);
        }
        assert!(Envelope::default().gains(0, 10).is_none());
        assert_eq!(db_to_gain(MIN_DB), 0.0);
    }

    #[test]
    fn retime_keeps_shape() {
        // Insert 50 samples at 0: everything moves right.
        let mut e = env();
        e.retime(0, 0, 50);
        assert_eq!(e.points[0].pos, 150);
        // Delete [250, 350): curve keeps -20 dB on both sides of the cut.
        let mut e = env();
        e.retime(250, 350, 250);
        assert_eq!(e.db_at(249.0), -20.0);
        assert!((e.db_at(250.0) + 10.0).abs() < 0.01, "{}", e.db_at(250.0));
        assert_eq!(e.db_at(300.0), 0.0);
        assert_eq!(e.points.last().unwrap().pos, 300);
        // Stretch [100, 400) to twice the length.
        let mut e = env();
        e.retime(100, 400, 700);
        assert_eq!(
            e.points.iter().map(|p| p.pos).collect::<Vec<_>>(),
            vec![100, 300, 500, 700]
        );
    }

    #[test]
    fn edit_points() {
        let mut e = env();
        let i = e.add(250, -6.0);
        assert_eq!(i, 2);
        let j = e.set(i, 50, 3.0);
        assert_eq!(j, 0);
        assert_eq!(e.points[0], EnvPoint { pos: 50, db: 3.0 });
        assert_eq!(e.add(10, 99.0), 0);
        assert_eq!(e.points[0].db, MAX_DB);
    }
}
