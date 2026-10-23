//! Auto-ducking: lower music (or any track) while someone is speaking on another track.
//!
//! The result is written as volume-envelope points rather than baked into the audio, so
//! the fades stay visible and editable with the Envelope tool.

use serde::{Deserialize, Serialize};

use crate::envelope::{EnvPoint, Envelope};

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct DuckParams {
    /// How far to lower the ducked tracks (negative dB).
    pub amount_db: f32,
    /// Voice level that counts as speech (dBFS RMS over 20 ms).
    pub threshold_db: f32,
    /// Fade-down time before speech starts.
    pub fade_down_ms: f32,
    /// Fade-up time after speech ends.
    pub fade_up_ms: f32,
    /// Start fading this long before the first word, so it isn't masked.
    pub lead_ms: f32,
    /// Pauses shorter than this stay ducked (no "pumping" between words).
    pub hold_ms: f32,
}

impl Default for DuckParams {
    fn default() -> Self {
        Self {
            amount_db: -12.0,
            threshold_db: -35.0,
            fade_down_ms: 300.0,
            fade_up_ms: 700.0,
            lead_ms: 100.0,
            hold_ms: 800.0,
        }
    }
}

fn ms(rate: u32, ms: f32) -> usize {
    (ms.max(0.0) * rate as f32 / 1000.0) as usize
}

/// Merge regions separated by less than `gap` samples.
fn merge(mut r: Vec<(usize, usize)>, gap: usize) -> Vec<(usize, usize)> {
    r.sort_unstable();
    let mut out: Vec<(usize, usize)> = Vec::new();
    for (s, e) in r {
        match out.last_mut() {
            Some(last) if s <= last.1 + gap => last.1 = last.1.max(e),
            _ => out.push((s, e)),
        }
    }
    out
}

/// Find where the control signal (e.g. a voice track, mono) is active.
pub fn detect_speech(
    control: &[f32],
    rate: u32,
    threshold_db: f32,
    hold_ms: f32,
) -> Vec<(usize, usize)> {
    let win = ms(rate, 20.0).max(1);
    let thresh = 10f32.powf(threshold_db / 10.0); // power threshold
    let mut regions = Vec::new();
    let mut i = 0;
    while i < control.len() {
        let end = (i + win).min(control.len());
        let p = control[i..end].iter().map(|x| x * x).sum::<f32>() / (end - i) as f32;
        if p > thresh {
            regions.push((i, end));
        }
        i = end;
    }
    let merged = merge(regions, ms(rate, hold_ms));
    // Ignore blips shorter than 60 ms (clicks, breaths).
    merged
        .into_iter()
        .filter(|(s, e)| e - s >= ms(rate, 60.0))
        .collect()
}

/// Fraction of 20 ms windows quieter than −45 dBFS. Speech has many pauses; music has few,
/// so the track with the highest value is a good guess for "the voice".
pub fn pause_fraction(channel: &[f32], rate: u32) -> f32 {
    let win = ms(rate, 20.0).max(1);
    let thresh = 10f32.powf(-45.0 / 10.0);
    let (mut quiet, mut total) = (0usize, 0usize);
    // Look at every 4th window: plenty for a guess and fast on long tracks.
    for chunk in channel.chunks(win).step_by(4) {
        let p = chunk.iter().map(|x| x * x).sum::<f32>() / chunk.len() as f32;
        total += 1;
        if p < thresh {
            quiet += 1;
        }
    }
    if total == 0 {
        0.0
    } else {
        quiet as f32 / total as f32
    }
}

/// Speech regions from transcript word timings.
pub fn regions_from_words(
    words: &[(usize, usize)],
    rate: u32,
    hold_ms: f32,
) -> Vec<(usize, usize)> {
    merge(words.to_vec(), ms(rate, hold_ms))
}

/// Envelope that dips by `amount_db` around each region, for a track of `len` samples.
pub fn duck_envelope(
    regions: &[(usize, usize)],
    len: usize,
    rate: u32,
    p: &DuckParams,
) -> Envelope {
    let (down, up, lead) = (
        ms(rate, p.fade_down_ms),
        ms(rate, p.fade_up_ms),
        ms(rate, p.lead_ms),
    );
    // Regions so close that their fades would overlap are joined.
    let joined = merge(regions.to_vec(), down + up + lead);
    let amount = p.amount_db.min(0.0);
    let mut pts = Vec::new();
    for (s, e) in joined {
        let low_start = s.saturating_sub(lead);
        let fade_start = low_start.saturating_sub(down);
        let low_end = e.min(len);
        let fade_end = (low_end + up).min(len.max(low_end));
        if fade_start < low_start {
            pts.push(EnvPoint {
                pos: fade_start,
                db: 0.0,
            });
        }
        pts.push(EnvPoint {
            pos: low_start,
            db: amount,
        });
        pts.push(EnvPoint {
            pos: low_end,
            db: amount,
        });
        if fade_end > low_end {
            pts.push(EnvPoint {
                pos: fade_end,
                db: 0.0,
            });
        }
    }
    Envelope::from_points(pts)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ducks_under_speech_only() {
        let rate = 1000;
        // Speech at 1.0–2.0 s and 2.3–3.0 s (gap 0.3 s < hold), then 6.0–7.0 s.
        let mut voice = vec![0.0f32; 10_000];
        for r in [(1000, 2000), (2300, 3000), (6000, 7000)] {
            for x in &mut voice[r.0..r.1] {
                *x = 0.3;
            }
        }
        let p = DuckParams::default();
        let regions = detect_speech(&voice, rate, p.threshold_db, p.hold_ms);
        assert_eq!(regions, vec![(1000, 3000), (6000, 7000)]);
        let env = duck_envelope(&regions, 10_000, rate, &p);
        assert_eq!(env.db_at(500.0), 0.0);
        assert_eq!(env.db_at(1500.0), -12.0);
        assert_eq!(env.db_at(2200.0), -12.0, "short pause stays ducked");
        assert_eq!(env.db_at(4500.0), 0.0, "music comes back between sentences");
        assert!(
            env.db_at(3350.0) > -12.0 && env.db_at(3350.0) < 0.0,
            "fades up"
        );
        assert!(
            env.db_at(5750.0) > -12.0 && env.db_at(5750.0) < 0.0,
            "fades down early"
        );
        assert_eq!(env.db_at(9900.0), 0.0);
        // Words from a transcript give the same kind of regions.
        let w = regions_from_words(&[(1000, 1400), (1500, 2000), (6000, 6500)], rate, 800.0);
        assert_eq!(w, vec![(1000, 2000), (6000, 6500)]);
    }
}
