//! Keyboard shortcuts: every command with its default keys, user changes (Edit ▸
//! Preferences ▸ Keyboard), and matching key presses to commands.

use std::collections::HashMap;

use eframe::egui::{self, Key, KeyboardShortcut, Modifiers};
use serde::{Deserialize, Serialize};

const KEY_STORAGE: &str = "keymap";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Act {
    Undo,
    Redo,
    NewProject,
    OpenProject,
    SaveProject,
    SaveProjectAs,
    ImportAudio,
    ExportAudio,
    Preferences,
    Cut,
    Copy,
    Paste,
    Delete,
    Silence,
    Trim,
    Duplicate,
    SelectAll,
    SplitClip,
    JoinClips,
    AddLabel,
    LabelsWindow,
    TranscriptPanel,
    ToolSelect,
    ToolTimeShift,
    ToolEnvelope,
    ZoomIn,
    ZoomOut,
    ZoomSelection,
    ZoomFit,
    PlayStop,
    Pause,
    Record,
    Loop,
    CursorStart,
    CursorEnd,
    CursorLeft,
    CursorRight,
}

/// One key combination. `command` is Ctrl on Windows/Linux and ⌘ on macOS.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Binding {
    pub command: bool,
    pub shift: bool,
    pub alt: bool,
    pub key: String,
}

impl Binding {
    fn new(m: Modifiers, key: Key) -> Self {
        Self {
            command: m.command,
            shift: m.shift,
            alt: m.alt,
            key: key.name().to_string(),
        }
    }

    pub fn shortcut(&self) -> Option<KeyboardShortcut> {
        let key = Key::from_name(&self.key)?;
        let mut m = Modifiers::NONE;
        if self.command {
            m |= Modifiers::COMMAND;
        }
        if self.shift {
            m |= Modifiers::SHIFT;
        }
        if self.alt {
            m |= Modifiers::ALT;
        }
        Some(KeyboardShortcut::new(m, key))
    }

    pub fn label(&self) -> String {
        let mut s = String::new();
        if self.command {
            s.push_str(if cfg!(target_os = "macos") {
                "⌘"
            } else {
                "Ctrl+"
            });
        }
        if self.alt {
            s.push_str(if cfg!(target_os = "macos") {
                "⌥"
            } else {
                "Alt+"
            });
        }
        if self.shift {
            s.push_str(if cfg!(target_os = "macos") {
                "⇧"
            } else {
                "Shift+"
            });
        }
        s.push_str(match self.key.as_str() {
            "Space" => "Space",
            "Delete" => "Del",
            k => k,
        });
        s
    }

    fn modifier_count(&self) -> usize {
        self.command as usize + self.shift as usize + self.alt as usize
    }
}

/// (command, menu label, default keys)
fn defaults() -> Vec<(Act, &'static str, Vec<Binding>)> {
    use Act::*;
    let c = Modifiers::COMMAND;
    let cs = Modifiers::COMMAND | Modifiers::SHIFT;
    let n = Modifiers::NONE;
    let b = Binding::new;
    vec![
        (Undo, "Undo", vec![b(c, Key::Z)]),
        (Redo, "Redo", vec![b(cs, Key::Z), b(c, Key::Y)]),
        (NewProject, "New project", vec![b(c, Key::N)]),
        (OpenProject, "Open project", vec![b(c, Key::O)]),
        (SaveProject, "Save project", vec![b(c, Key::S)]),
        (SaveProjectAs, "Save project as", vec![b(cs, Key::S)]),
        (ImportAudio, "Import audio", vec![b(cs, Key::I)]),
        (ExportAudio, "Export audio", vec![b(cs, Key::E)]),
        (Preferences, "Preferences", vec![b(c, Key::P)]),
        (Cut, "Cut", vec![b(c, Key::X)]),
        (Copy, "Copy", vec![b(c, Key::C)]),
        (Paste, "Paste", vec![b(c, Key::V)]),
        (
            Delete,
            "Delete",
            vec![b(n, Key::Delete), b(n, Key::Backspace)],
        ),
        (Silence, "Silence audio", vec![b(c, Key::L)]),
        (Trim, "Trim audio", vec![b(c, Key::T)]),
        (Duplicate, "Duplicate", vec![b(c, Key::D)]),
        (SelectAll, "Select all", vec![b(c, Key::A)]),
        (SplitClip, "Split clip", vec![b(c, Key::I)]),
        (JoinClips, "Join clips", vec![b(c, Key::J)]),
        (AddLabel, "Add label", vec![b(c, Key::B)]),
        (LabelsWindow, "Labels window", vec![b(cs, Key::B)]),
        (TranscriptPanel, "Transcript panel", vec![b(cs, Key::T)]),
        (ToolSelect, "Selection tool", vec![b(n, Key::F1)]),
        (ToolTimeShift, "Time Shift tool", vec![b(n, Key::F5)]),
        (ToolEnvelope, "Envelope tool", vec![b(n, Key::F2)]),
        (ZoomIn, "Zoom in", vec![b(c, Key::Num1)]),
        (ZoomOut, "Zoom out", vec![b(c, Key::Num3)]),
        (ZoomSelection, "Zoom to selection", vec![b(c, Key::E)]),
        (ZoomFit, "Fit project in window", vec![b(c, Key::F)]),
        (PlayStop, "Play / Stop", vec![b(n, Key::Space)]),
        (Pause, "Pause", vec![b(n, Key::P)]),
        (Record, "Record", vec![b(n, Key::R)]),
        (Loop, "Loop on/off", vec![b(n, Key::L)]),
        (
            CursorStart,
            "Cursor to start (Shift: select)",
            vec![b(n, Key::Home), b(n, Key::J)],
        ),
        (
            CursorEnd,
            "Cursor to end (Shift: select)",
            vec![b(n, Key::End), b(n, Key::K)],
        ),
        (CursorLeft, "Cursor left", vec![b(n, Key::ArrowLeft)]),
        (CursorRight, "Cursor right", vec![b(n, Key::ArrowRight)]),
    ]
}

pub struct Keymap {
    map: HashMap<Act, Vec<Binding>>,
    /// Command whose keys are being changed (waiting for a key press).
    pub capturing: Option<Act>,
    pub note: Option<String>,
}

impl Keymap {
    pub fn load(storage: Option<&dyn eframe::Storage>) -> Self {
        let mut map: HashMap<Act, Vec<Binding>> =
            defaults().into_iter().map(|(a, _, b)| (a, b)).collect();
        if let Some(saved) =
            storage.and_then(|s| eframe::get_value::<Vec<(Act, Vec<Binding>)>>(s, KEY_STORAGE))
        {
            for (a, b) in saved {
                map.insert(a, b);
            }
        }
        Self {
            map,
            capturing: None,
            note: None,
        }
    }

    pub fn save(&self, storage: &mut dyn eframe::Storage) {
        let v: Vec<(Act, Vec<Binding>)> = self.map.iter().map(|(a, b)| (*a, b.clone())).collect();
        eframe::set_value(storage, KEY_STORAGE, &v);
    }

    /// Menu text for a command's first key, e.g. "Ctrl+Z".
    pub fn label(&self, act: Act) -> String {
        self.map
            .get(&act)
            .and_then(|b| b.first())
            .map(Binding::label)
            .unwrap_or_default()
    }

    pub fn labels(&self, act: Act) -> String {
        self.map
            .get(&act)
            .map(|b| b.iter().map(Binding::label).collect::<Vec<_>>().join(", "))
            .unwrap_or_default()
    }

    /// The commands whose keys were pressed this frame. Combinations with more modifiers
    /// are checked first, so Ctrl+Shift+Z isn't taken for Ctrl+Z.
    pub fn pressed(&self, ctx: &egui::Context) -> Vec<Act> {
        let mut all: Vec<(Act, &Binding)> = self
            .map
            .iter()
            .flat_map(|(a, bs)| bs.iter().map(move |b| (*a, b)))
            .collect();
        all.sort_by_key(|(_, b)| std::cmp::Reverse(b.modifier_count()));
        let mut out = Vec::new();
        for (a, b) in all {
            if let Some(sc) = b.shortcut()
                && ctx.input_mut(|i| i.consume_shortcut(&sc))
                && !out.contains(&a)
            {
                out.push(a);
            }
        }
        out
    }

    /// While capturing, take the next key press as the command's new key.
    pub fn capture(&mut self, ctx: &egui::Context) {
        let Some(act) = self.capturing else { return };
        let ev = ctx.input(|i| {
            i.events.iter().find_map(|e| match e {
                egui::Event::Key {
                    key,
                    pressed: true,
                    modifiers,
                    ..
                } => Some((*key, *modifiers)),
                _ => None,
            })
        });
        let Some((key, m)) = ev else { return };
        if key == Key::Escape {
            self.capturing = None;
            return;
        }
        self.assign(act, Binding::new(m, key));
        self.capturing = None;
        ctx.input_mut(|i| i.events.clear());
    }

    /// Make `b` the key for `act`, taking it away from any other command.
    fn assign(&mut self, act: Act, b: Binding) {
        let mut taken_from = Vec::new();
        for (a, bs) in self.map.iter_mut() {
            if *a != act && bs.contains(&b) {
                bs.retain(|x| *x != b);
                taken_from.push(*a);
            }
        }
        self.note = (!taken_from.is_empty()).then(|| {
            let names: Vec<&str> = defaults()
                .into_iter()
                .filter(|(a, _, _)| taken_from.contains(a))
                .map(|(_, n, _)| n)
                .collect();
            format!(
                "{} was used by {}; it now does this instead.",
                b.label(),
                names.join(", ")
            )
        });
        self.map.insert(act, vec![b]);
    }

    pub fn reset(&mut self, act: Act) {
        if let Some((_, _, b)) = defaults().into_iter().find(|(a, _, _)| *a == act) {
            self.map.insert(act, b);
        }
    }

    pub fn reset_all(&mut self) {
        self.map = defaults().into_iter().map(|(a, _, b)| (a, b)).collect();
        self.note = None;
    }

    pub fn clear(&mut self, act: Act) {
        self.map.insert(act, Vec::new());
    }

    pub fn is_default(&self, act: Act) -> bool {
        defaults()
            .into_iter()
            .find(|(a, _, _)| *a == act)
            .map(|(_, _, b)| b)
            == self.map.get(&act).cloned()
    }

    /// (command, label) in display order.
    pub fn commands() -> Vec<(Act, &'static str)> {
        defaults().into_iter().map(|(a, n, _)| (a, n)).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rebinding_moves_a_key_and_round_trips() {
        let mut k = Keymap::load(None);
        assert_eq!(
            k.label(Act::Undo),
            if cfg!(target_os = "macos") {
                "⌘Z"
            } else {
                "Ctrl+Z"
            }
        );
        // Give Ctrl+I (Split clip) to Trim.
        k.assign(Act::Trim, Binding::new(Modifiers::COMMAND, Key::I));
        assert!(k.labels(Act::SplitClip).is_empty());
        assert!(k.label(Act::Trim).ends_with('I'));
        assert!(k.note.is_some());
        k.reset_all();
        assert!(k.is_default(Act::Trim) && k.is_default(Act::SplitClip));
        for (a, _) in Keymap::commands() {
            for b in k.map.get(&a).unwrap() {
                assert!(b.shortcut().is_some(), "{a:?} {b:?}");
            }
        }
    }
}
