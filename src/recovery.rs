//! Autosave and crash recovery.
//!
//! Each running app owns a *session* in the recovery folder: `<id>.lock` (held open with an
//! exclusive lock for as long as the app runs), `<id>.rap` (the latest autosave) and
//! `<id>.json` (what it is: title, original project path, time). A clean exit deletes all
//! three. If the app crashes, the OS releases the lock, so on the next start any session
//! whose lock can be taken is an orphan that can be recovered.

use std::{
    fs::{self, File, OpenOptions},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::{
    project::Project,
    projectfile::{self, ViewState},
};

/// Folder for app data (recovery files, …). `RUST_AUDACITY_DATA_DIR` overrides it.
pub fn data_dir() -> PathBuf {
    if let Some(d) = std::env::var_os("RUST_AUDACITY_DATA_DIR") {
        return PathBuf::from(d);
    }
    let home = || {
        std::env::var_os("HOME")
            .or_else(|| std::env::var_os("USERPROFILE"))
            .map(PathBuf::from)
    };
    if cfg!(windows) {
        if let Some(d) = std::env::var_os("LOCALAPPDATA") {
            return PathBuf::from(d).join("Rust Audacity");
        }
    } else if cfg!(target_os = "macos") {
        if let Some(h) = home() {
            return h.join("Library/Application Support/Rust Audacity");
        }
    } else if let Some(d) = std::env::var_os("XDG_DATA_HOME") {
        return PathBuf::from(d).join("rust-audacity");
    } else if let Some(h) = home() {
        return h.join(".local/share/rust-audacity");
    }
    std::env::temp_dir().join("rust-audacity")
}

pub fn recovery_dir() -> PathBuf {
    data_dir().join("recovery")
}

/// What an autosave contains, shown when offering recovery.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Info {
    pub title: String,
    /// The project file it was opened from / saved to, if any.
    pub project_path: Option<PathBuf>,
    /// Unix seconds.
    pub saved: u64,
    pub tracks: usize,
    pub seconds: f64,
}

impl Info {
    pub fn describe(&self, p: &Project) -> Self {
        let mut i = self.clone();
        i.tracks = p.tracks.len();
        i.seconds = p.len() as f64 / p.rate.max(1) as f64;
        i.saved = now();
        i
    }
}

pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// This app's own recovery session.
pub struct Session {
    dir: PathBuf,
    id: String,
    lock: Option<File>,
}

impl Session {
    pub fn start(dir: &Path) -> Result<Self> {
        fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.subsec_nanos());
        let id = format!("{}-{}-{nanos:09}", now(), std::process::id());
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(dir.join(format!("{id}.lock")))?;
        lock.try_lock()
            .map_err(|e| anyhow::anyhow!("locking the recovery session: {e:?}"))?;
        Ok(Self {
            dir: dir.to_path_buf(),
            id,
            lock: Some(lock),
        })
    }

    pub fn rap_path(&self) -> PathBuf {
        self.dir.join(format!("{}.rap", self.id))
    }
    fn json_path(&self) -> PathBuf {
        self.dir.join(format!("{}.json", self.id))
    }

    /// Where this session's autosave goes; can be sent to a background thread.
    pub fn target(&self) -> Target {
        Target {
            dir: self.dir.clone(),
            id: self.id.clone(),
        }
    }

    pub fn write(&self, project: &Project, view: Option<ViewState>, info: &Info) -> Result<()> {
        self.target().write(project, view, info)
    }

    /// Forget the autosave (after a normal save, or "New").
    pub fn clear(&self) {
        let _ = fs::remove_file(self.json_path());
        let _ = fs::remove_file(self.rap_path());
    }

    pub fn has_autosave(&self) -> bool {
        self.json_path().exists()
    }

    /// Clean exit: remove everything, including the lock file.
    pub fn finish(mut self) {
        self.clear();
        drop(self.lock.take());
        let _ = fs::remove_file(self.dir.join(format!("{}.lock", self.id)));
    }
}

/// The files of one session's autosave.
#[derive(Clone, Debug)]
pub struct Target {
    dir: PathBuf,
    id: String,
}

impl Target {
    /// Write an autosave. The project file goes first, so a JSON file always has a
    /// complete project next to it.
    pub fn write(&self, project: &Project, view: Option<ViewState>, info: &Info) -> Result<()> {
        projectfile::save(&self.dir.join(format!("{}.rap", self.id)), project, view)?;
        let tmp = self.dir.join(format!("{}.json.tmp", self.id));
        fs::write(&tmp, serde_json::to_vec_pretty(info)?)?;
        fs::rename(&tmp, self.dir.join(format!("{}.json", self.id)))?;
        Ok(())
    }
}

/// An autosave left behind by an app that didn't close normally.
#[derive(Clone, Debug)]
pub struct Orphan {
    pub id: String,
    pub info: Info,
    pub rap: PathBuf,
    dir: PathBuf,
}

impl Orphan {
    /// Delete its files.
    pub fn discard(&self) {
        for ext in ["rap", "json", "lock"] {
            let _ = fs::remove_file(self.dir.join(format!("{}.{ext}", self.id)));
        }
    }
}

/// Sessions in `dir` whose app is no longer running and that have an autosave, newest first.
/// Dead sessions without an autosave are cleaned up.
pub fn find_orphans(dir: &Path) -> Vec<Orphan> {
    let Ok(rd) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut ids: Vec<String> = rd
        .flatten()
        .filter_map(|e| {
            let p = e.path();
            let ext = p.extension()?.to_str()?;
            (ext == "lock" || ext == "json").then(|| p.file_stem()?.to_str().map(str::to_string))?
        })
        .collect();
    ids.sort();
    ids.dedup();
    let mut out = Vec::new();
    for id in ids {
        let lock_path = dir.join(format!("{id}.lock"));
        if lock_path.exists() {
            let Ok(f) = OpenOptions::new().write(true).open(&lock_path) else {
                continue;
            };
            if f.try_lock().is_err() {
                continue; // that app is still running
            }
        }
        let json = dir.join(format!("{id}.json"));
        let rap = dir.join(format!("{id}.rap"));
        let info = fs::read(&json)
            .ok()
            .and_then(|b| serde_json::from_slice::<Info>(&b).ok());
        match info {
            Some(info) if rap.exists() => out.push(Orphan {
                id,
                info,
                rap,
                dir: dir.to_path_buf(),
            }),
            _ => {
                for ext in ["rap", "json", "lock", "rap.tmp", "json.tmp"] {
                    let _ = fs::remove_file(dir.join(format!("{id}.{ext}")));
                }
            }
        }
    }
    out.sort_by_key(|o| std::cmp::Reverse(o.info.saved));
    out
}

/// Recently used files, most recent first.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct RecentFiles(pub Vec<PathBuf>);

impl RecentFiles {
    pub const MAX: usize = 12;

    pub fn add(&mut self, p: &Path) {
        let p = fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
        // Strip Windows' \\?\ prefix that canonicalize adds.
        let p = match p.to_str().and_then(|s| s.strip_prefix(r"\\?\")) {
            Some(s) => PathBuf::from(s),
            None => p,
        };
        self.0.retain(|q| q != &p);
        self.0.insert(0, p);
        self.0.truncate(Self::MAX);
    }

    pub fn remove(&mut self, p: &Path) {
        self.0.retain(|q| q != p);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crashed_sessions_are_found_and_live_ones_are_not() {
        let dir = std::env::temp_dir().join(format!("ra_recovery_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let mut p = Project::default();
        p.add_track("voice", vec![vec![0.25; 4410]]);
        let info = Info {
            title: "Episode 3".into(),
            project_path: None,
            saved: 0,
            tracks: 0,
            seconds: 0.0,
        }
        .describe(&p);

        // A running session with an autosave is not an orphan.
        let live = Session::start(&dir).unwrap();
        live.write(&p, None, &info).unwrap();
        assert!(find_orphans(&dir).is_empty());

        // "Crash": the lock is released but the files stay.
        let crashed_id = live.id.clone();
        drop(live);
        let orphans = find_orphans(&dir);
        assert_eq!(orphans.len(), 1);
        assert_eq!(orphans[0].id, crashed_id);
        assert_eq!(orphans[0].info.title, "Episode 3");
        assert_eq!(orphans[0].info.tracks, 1);
        let loaded = projectfile::load(&orphans[0].rap).unwrap();
        assert_eq!(loaded.project.tracks[0].len(), 4410);
        orphans[0].discard();
        assert!(find_orphans(&dir).is_empty());

        // A clean exit leaves nothing behind; a session that never autosaved is cleaned up.
        let s = Session::start(&dir).unwrap();
        s.write(&p, None, &info).unwrap();
        s.finish();
        let quiet = Session::start(&dir).unwrap();
        drop(quiet);
        assert!(find_orphans(&dir).is_empty());
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 0);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn recent_files_dedup_and_cap() {
        let mut r = RecentFiles::default();
        for i in 0..20 {
            r.add(Path::new(&format!("/nope/{i}.wav")));
        }
        r.add(Path::new("/nope/5.wav"));
        assert_eq!(r.0.len(), RecentFiles::MAX);
        assert_eq!(r.0[0], PathBuf::from("/nope/5.wav"));
        assert_eq!(r.0.iter().filter(|p| p.ends_with("5.wav")).count(), 1);
    }
}
