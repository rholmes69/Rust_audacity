//! `app_*` tools: drive a running Rust Audacity window through the agent bridge
//! (`rust_audacity::bridge`). Everything happens in the open window, so the user can
//! watch, listen, undo and keep editing by hand.

use std::time::{Duration, Instant};

use crate::{
    bridge::Client,
    effects::{Effect, builtin_chains},
};
use serde_json::{Value, json};

use super::{
    Config, Progress,
    tools::{Output, R, b, f, input_path, norm, output_path, parse_effect, req_str, s},
};

pub const NAMES: [&str; 11] = [
    "app_auto_duck",
    "app_status",
    "app_open",
    "app_select",
    "app_edit",
    "app_apply_effects",
    "app_track",
    "app_playback",
    "app_export",
    "app_save_project",
    "app_transcript",
];

pub fn exists(name: &str) -> bool {
    NAMES.contains(&name)
}

fn tool(
    name: &str,
    title: &str,
    description: &str,
    props: Value,
    required: &[&str],
    read_only: bool,
) -> Value {
    json!({
        "name": name,
        "title": title,
        "description": description,
        "inputSchema": { "type": "object", "properties": props, "required": required, "additionalProperties": false },
        "annotations": { "title": title, "readOnlyHint": read_only, "destructiveHint": false, "idempotentHint": read_only, "openWorldHint": false },
    })
}

pub fn definitions() -> Vec<Value> {
    let secs = |d: &str| json!({ "type": "number", "description": d });
    vec![
        tool(
            "app_status",
            "Rust Audacity window: status",
            "Describe what is open in the running Rust Audacity app: tracks (index, name, duration, gain, mute/solo, selected), \
             the time selection, whether a job is running, undo history and transcript info. Set measure=true to also get loudness \
             (LUFS, peak) of the selection (or everything). Call this first; all app_* tools need the app to be open.",
            json!({ "measure": { "type": "boolean", "description": "Also measure loudness of the selection / whole project." } }),
            &[],
            true,
        ),
        tool(
            "app_open",
            "Rust Audacity window: open file",
            "Open an audio file as a new track (WAV, MP3, FLAC, OGG, M4A…) or open a .rap project in the running app. Waits until the import finishes.",
            json!({
                "path": { "type": "string", "description": "Audio file or .rap project." },
                "discard_changes": { "type": "boolean", "description": "Opening a .rap replaces the current project; set true to allow losing unsaved changes." },
            }),
            &["path"],
            false,
        ),
        tool(
            "app_select",
            "Rust Audacity window: select",
            "Set the time selection (seconds) and which tracks are selected. Edits and effects act on the selected tracks within the \
             selection (or the whole track when the selection is empty). Omit start/end to select everything.",
            json!({
                "start": secs("Start in seconds (default 0)."),
                "end": secs("End in seconds (default: end of project)."),
                "tracks": { "description": "Track indices to select, or \"all\". Omit to keep the current track selection.", "anyOf": [{ "type": "array", "items": { "type": "integer" } }, { "type": "string", "enum": ["all"] }] },
                "zoom": { "type": "boolean", "description": "Also zoom the view to the selection." },
            }),
            &[],
            false,
        ),
        tool(
            "app_edit",
            "Rust Audacity window: edit",
            "Run an edit command on the selection: cut, copy, paste (at the selection start), delete, silence, trim (keep only the \
             selection), duplicate / split_new (into a new track), mix_and_render, select_all, remove_selected_tracks, undo, redo.",
            json!({ "op": { "type": "string", "enum": ["cut", "copy", "paste", "delete", "silence", "trim", "duplicate", "split_new", "mix_and_render", "select_all", "remove_selected_tracks", "undo", "redo"] } }),
            &["op"],
            false,
        ),
        tool(
            "app_apply_effects",
            "Rust Audacity window: apply effects",
            "Apply effects (same specs as apply_effects / list_effects) and/or a built-in chain to the selected tracks over the \
             selection, as ONE undo step, then wait for it to finish and report loudness. Noise reduction takes its noise sample \
             automatically from the quietest half-second if no profile was captured.",
            json!({
                "effects": { "type": "array", "description": "Effect ids or {\"effect\": id, \"params\": {...}} objects, applied in order.", "items": {} },
                "chain": { "type": "string", "description": "Name of a built-in chain, e.g. \"Podcast Voice\"." },
            }),
            &[],
            false,
        ),
        tool(
            "app_track",
            "Rust Audacity window: track settings",
            "Change a track's name, gain (dB), pan (-1 left … 1 right), mute, solo or selected state, or set its volume \
             envelope: `envelope` = [[seconds, dB], …] points joined by straight lines (0 dB = unchanged, -60 = silent); [] clears it.",
            json!({
                "index": { "type": "integer" },
                "name": { "type": "string" },
                "gain_db": secs("Track gain in dB (-60…36)."),
                "pan": secs("-1 (left) … 1 (right)."),
                "mute": { "type": "boolean" },
                "solo": { "type": "boolean" },
                "selected": { "type": "boolean" },
                "envelope": { "type": "array", "items": { "type": "array", "items": { "type": "number" } }, "description": "Volume envelope points [[seconds, dB], …]; [] removes the envelope." },
            }),
            &["index"],
            false,
        ),
        tool(
            "app_auto_duck",
            "Rust Audacity window: auto duck",
            "Lower music (or any tracks) automatically while someone speaks on the voice track, by writing volume-envelope \
             points the user can see and adjust (Envelope tool, F2). Uses the transcript's word timings when the voice track \
             has been transcribed, otherwise the voice level. Replaces existing envelopes on the ducked tracks; one undo step.",
            json!({
                "voice_track": { "type": "integer", "description": "Index of the speech track (default: the transcribed track)." },
                "duck_tracks": { "type": "array", "items": { "type": "integer" }, "description": "Tracks to lower (default: all others)." },
                "amount_db": secs("How much to lower, in dB (default 12)."),
                "threshold_db": secs("Voice level counted as speech, dBFS (default -35; lower = more sensitive)."),
                "fade_down_ms": secs("Fade-down time (default 300)."),
                "fade_up_ms": secs("Fade-up time (default 700)."),
                "hold_ms": secs("Stay down through pauses shorter than this (default 800)."),
                "use_transcript": { "type": "boolean", "description": "Use transcript words if available (default true)." },
            }),
            &[],
            false,
        ),
        tool(
            "app_playback",
            "Rust Audacity window: play / stop",
            "Play the project through the user's speakers from `start` (default: selection start) to `end`, or stop.",
            json!({
                "action": { "type": "string", "enum": ["play", "stop"] },
                "start": secs("Start in seconds."),
                "end": secs("Stop at this time (seconds)."),
            }),
            &["action"],
            false,
        ),
        tool(
            "app_export",
            "Rust Audacity window: export",
            "Export the mix (or only the selection) to a file; the extension picks the format (.wav, .flac, .mp3, .ogg). Waits until done.",
            json!({
                "path": { "type": "string" },
                "selection_only": { "type": "boolean" },
                "wav_format": { "type": "string", "enum": ["pcm16", "pcm24", "float32"], "description": "WAV/FLAC sample format (default pcm16)." },
                "mp3_kbps": { "type": "integer", "enum": [64, 96, 128, 160, 192, 224, 256, 320] },
                "ogg_quality": { "type": "integer", "minimum": 0, "maximum": 10 },
            }),
            &["path"],
            false,
        ),
        tool(
            "app_save_project",
            "Rust Audacity window: save project",
            "Save the open project as a .rap file (audio, tracks, transcript). Without `path` it saves to the project's current file.",
            json!({ "path": { "type": "string", "description": "Where to save (.rap). Required the first time." } }),
            &[],
            false,
        ),
        tool(
            "app_transcript",
            "Rust Audacity window: transcript",
            "Speech-to-text and text-based editing. action=transcribe runs Whisper on a track (default: first selected) and waits; \
             action=get returns the words as [index, start, end, text] (optionally only between start/end seconds) plus plain text; \
             action=delete_words cuts words from..to (inclusive word indices) out of the audio — on all tracks unless all_tracks=false; \
             action=remove_fillers cuts every um/uh/erm; action=show opens the transcript panel. Re-run get after deleting: indices shift.",
            json!({
                "action": { "type": "string", "enum": ["transcribe", "get", "delete_words", "remove_fillers", "show"] },
                "track": { "type": "integer", "description": "transcribe: track index." },
                "model": { "type": "string", "enum": ["tiny", "base", "small", "medium", "turbo", "large-v3"], "description": "transcribe: Whisper model (default: the app's setting, usually small)." },
                "language": { "type": "string", "description": "transcribe: language code like \"en\" (default: detect)." },
                "start": secs("get: only words after this time."),
                "end": secs("get: only words before this time."),
                "from": { "type": "integer", "description": "delete_words: first word index." },
                "to": { "type": "integer", "description": "delete_words: last word index (inclusive)." },
                "all_tracks": { "type": "boolean", "description": "delete_words/remove_fillers: also cut the other tracks to keep them in sync (default true)." },
            }),
            &["action"],
            false,
        ),
    ]
}

/// Poll until the app is idle, reporting progress. Returns the final status.
fn wait_idle(c: &mut Client, progress: &Progress, what: &str, timeout: Duration) -> R<Value> {
    let started = Instant::now();
    // Give the app a frame to pick the job up.
    std::thread::sleep(Duration::from_millis(150));
    loop {
        let st = c.call("status", json!({}))?;
        if !st["busy"].as_bool().unwrap_or(false) {
            if let Some(m) = st["dialog_message"].as_str() {
                let m = m.to_string();
                let _ = c.call("dismiss_dialog", json!({}));
                return Err(m);
            }
            return Ok(st);
        }
        if started.elapsed() > timeout {
            return Err(format!(
                "{what} is still running after {} s; check app_status later",
                timeout.as_secs()
            ));
        }
        let p = st["job"]["progress"].as_f64();
        progress.report(
            p.unwrap_or(0.0),
            p.map(|_| 100.0),
            st["job"]["label"].as_str().unwrap_or(what),
        );
        std::thread::sleep(Duration::from_millis(300));
    }
}

fn track_lines(st: &Value) -> String {
    st["tracks"]
        .as_array()
        .map(|ts| {
            ts.iter()
                .map(|t| {
                    format!(
                        "  [{}] {} – {:.2}s, {} ch, gain {} dB{}{}{}",
                        t["index"],
                        t["name"].as_str().unwrap_or(""),
                        t["duration"].as_f64().unwrap_or(0.0),
                        t["channels"],
                        t["gain_db"],
                        if t["mute"].as_bool() == Some(true) {
                            ", muted"
                        } else {
                            ""
                        },
                        if t["solo"].as_bool() == Some(true) {
                            ", solo"
                        } else {
                            ""
                        },
                        if t["selected"].as_bool() == Some(true) {
                            ", selected"
                        } else {
                            ""
                        },
                    )
                })
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default()
}

fn status_text(st: &Value) -> String {
    let sel = &st["selection"];
    let mut t = format!(
        "“{}”{} – {:.2}s at {} Hz, {} track(s)\n{}\nSelection: {}–{} s",
        st["title"].as_str().unwrap_or(""),
        if st["unsaved_changes"].as_bool() == Some(true) {
            " (unsaved)"
        } else {
            ""
        },
        st["duration"].as_f64().unwrap_or(0.0),
        st["sample_rate"],
        st["tracks"].as_array().map_or(0, Vec::len),
        track_lines(st),
        sel["start"],
        sel["end"],
    );
    if let Some(l) = st.get("levels").filter(|l| !l.is_null()) {
        t.push_str(&format!(
            "\nLevels ({}–{} s): {} LUFS, peak {} dBFS, true peak {} dBTP",
            l["range"][0], l["range"][1], l["lufs"], l["peak_dbfs"], l["true_peak_dbtp"]
        ));
    }
    if let Some(tr) = st.get("transcript").filter(|t| !t.is_null()) {
        t.push_str(&format!(
            "\nTranscript: {} words on track {}",
            tr["words"], tr["track"]
        ));
    }
    if st["busy"].as_bool() == Some(true) {
        t.push_str("\nBusy: ");
        t.push_str(st["job"]["label"].as_str().unwrap_or("working"));
    }
    t
}

fn out(text: String, data: Value) -> R<Output> {
    Ok(Output {
        text,
        data,
        png: None,
    })
}

pub fn call(name: &str, args: &Value, cfg: &Config, progress: &Progress) -> R<Output> {
    let mut c = match &cfg.bridge {
        Some(info) => Client::connect_to(info)?,
        None => Client::connect()?,
    };
    match name {
        "app_status" => {
            let st = c.call("status", json!({ "measure": b(args, "measure") }))?;
            out(status_text(&st), st)
        }
        "app_open" => {
            let path = input_path(cfg, req_str(args, "path")?)?;
            let r = c.call(
                "open",
                json!({ "path": path.display().to_string(), "discard_changes": b(args, "discard_changes") }),
            )?;
            let st = wait_idle(&mut c, progress, "Import", Duration::from_secs(600))?;
            out(
                format!("Opened {}\n{}", path.display(), status_text(&st)),
                json!({ "result": r, "status": st }),
            )
        }
        "app_select" => {
            let mut a = json!({});
            for k in ["start", "end", "zoom"] {
                if let Some(v) = args.get(k) {
                    a[k] = v.clone();
                }
            }
            if let Some(t) = args.get("tracks") {
                a["tracks"] = t.clone();
            }
            let r = c.call("select", a)?;
            let st = c.call("status", json!({}))?;
            out(status_text(&st), json!({ "result": r, "status": st }))
        }
        "app_edit" => {
            let op = req_str(args, "op")?;
            let r = c.call("edit", json!({ "op": op }))?;
            let st = c.call("status", json!({}))?;
            out(
                format!(
                    "{op}: done (undo with app_edit op=undo)\n{}",
                    status_text(&st)
                ),
                json!({ "result": r, "status": st }),
            )
        }
        "app_apply_effects" => {
            let mut steps: Vec<Effect> = Vec::new();
            if let Some(chain) = s(args, "chain") {
                let ch = builtin_chains()
                    .into_iter()
                    .find(|c| {
                        norm(&c.name) == norm(chain) || norm(&c.name).starts_with(&norm(chain))
                    })
                    .ok_or_else(|| format!("unknown chain `{chain}`"))?;
                steps.extend(ch.steps);
            }
            if let Some(list) = args.get("effects") {
                for (i, spec) in list
                    .as_array()
                    .ok_or("`effects` must be an array")?
                    .iter()
                    .enumerate()
                {
                    steps.push(parse_effect(spec).map_err(|e| format!("effects[{i}]: {e}"))?);
                }
            }
            if steps.is_empty() {
                return Err("nothing to do: pass `effects` and/or `chain`".into());
            }
            let label = s(args, "chain").map(|c| format!("Chain “{c}”"));
            let r = c.call(
                "apply_effects",
                json!({ "effects": serde_json::to_value(&steps).map_err(|e| e.to_string())?, "label": label }),
            )?;
            wait_idle(&mut c, progress, "Effects", Duration::from_secs(1800))?;
            let st = c.call("status", json!({ "measure": true }))?;
            let mut text = format!(
                "Applied {} as one undo step.\n{}",
                r["started"].as_str().unwrap_or("effects"),
                status_text(&st)
            );
            if let Some(n) = r["note"].as_str() {
                text.push_str(&format!("\nNote: {n}"));
            }
            out(text, json!({ "result": r, "status": st }))
        }
        "app_track" => {
            let mut a = args.clone();
            let env = a.as_object_mut().and_then(|o| o.remove("envelope"));
            if let Some(points) = env {
                c.call(
                    "envelope",
                    json!({ "index": args["index"], "points": points }),
                )?;
            }
            let r = c.call("track", a)?;
            out(format!("Track updated: {r}"), r)
        }
        "app_auto_duck" => {
            let r = c.call("auto_duck", args.clone())?;
            out(
                format!(
                    "Ducked track(s) {} by {} dB during {} stretch(es) of speech on track {}. The envelope is visible in the app (Envelope tool, F2) and undoable.",
                    r["ducked_tracks"],
                    -r["amount_db"].as_f64().unwrap_or(12.0),
                    r["speech_regions"],
                    r["voice_track"]
                ),
                r,
            )
        }
        "app_playback" => {
            let action = req_str(args, "action")?;
            let r = match action {
                "play" => {
                    let mut a = json!({});
                    if let Some(x) = f(args, "start") {
                        a["start"] = json!(x);
                    }
                    if let Some(x) = f(args, "end") {
                        a["end"] = json!(x);
                    }
                    c.call("play", a)?
                }
                "stop" => c.call("stop", json!({}))?,
                o => return Err(format!("unknown action {o}")),
            };
            out(format!("{action}: {r}"), r)
        }
        "app_export" => {
            let raw = req_str(args, "path")?;
            let ext = std::path::Path::new(raw)
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or("wav")
                .to_string();
            let path = output_path(cfg, raw, &ext)?;
            let mut a = args.clone();
            a["path"] = json!(path.display().to_string());
            let r = c.call("export", a)?;
            let st = wait_idle(&mut c, progress, "Export", Duration::from_secs(1800))?;
            let _ = st;
            out(
                format!(
                    "Exported {} ({}).",
                    path.display(),
                    r["format"].as_str().unwrap_or("")
                ),
                json!({ "result": r, "path": path.display().to_string() }),
            )
        }
        "app_save_project" => {
            let mut a = json!({});
            if let Some(p) = s(args, "path") {
                a["path"] = json!(output_path(cfg, p, "rap")?.display().to_string());
            }
            let r = c.call("save_project", a)?;
            out(format!("Saved {}", r["saved"].as_str().unwrap_or("")), r)
        }
        "app_transcript" => {
            let action = req_str(args, "action")?;
            match action {
                "transcribe" => {
                    let mut a = json!({});
                    for k in ["track", "model", "language"] {
                        if let Some(v) = args.get(k) {
                            a[k] = v.clone();
                        }
                    }
                    c.call("transcribe", a)?;
                    c.call("show_transcript", json!({ "show": true }))?;
                    wait_idle(
                        &mut c,
                        progress,
                        "Transcription",
                        Duration::from_secs(3 * 3600),
                    )?;
                    let t = c.call("transcript", json!({}))?;
                    let text = t["text"].as_str().unwrap_or("").to_string();
                    out(
                        format!(
                            "Transcribed {} words (language {}). Use action=get for word indices and times.\n\n{}",
                            t["word_count"],
                            t["language"].as_str().unwrap_or("?"),
                            if text.len() > 6000 {
                                format!("{}…", &text[..text.floor_char_boundary(6000)])
                            } else {
                                text
                            }
                        ),
                        json!({ "word_count": t["word_count"], "language": t["language"] }),
                    )
                }
                "get" => {
                    let mut a = json!({});
                    for k in ["start", "end"] {
                        if let Some(v) = args.get(k) {
                            a[k] = v.clone();
                        }
                    }
                    let t = c.call("transcript", a)?;
                    let words = t["words"].as_array().cloned().unwrap_or_default();
                    let listing: Vec<String> = words
                        .iter()
                        .map(|w| format!("{}:{}@{}", w[0], w[3].as_str().unwrap_or(""), w[1]))
                        .collect();
                    out(
                        format!(
                            "{} of {} words (format index:word@start_seconds):\n{}",
                            words.len(),
                            t["word_count"],
                            listing.join(" ")
                        ),
                        t,
                    )
                }
                "delete_words" => {
                    let mut a = json!({});
                    for k in ["from", "to", "all_tracks"] {
                        if let Some(v) = args.get(k) {
                            a[k] = v.clone();
                        }
                    }
                    let r = c.call("delete_words", a)?;
                    out(
                        format!(
                            "Cut {} word(s): “{}”. Project is now {:.2}s. Word indices after this point have changed.",
                            r["deleted_words"],
                            r["text"].as_str().unwrap_or(""),
                            r["duration"].as_f64().unwrap_or(0.0)
                        ),
                        r,
                    )
                }
                "remove_fillers" => {
                    let mut a = json!({});
                    if let Some(v) = args.get("all_tracks") {
                        a["all_tracks"] = v.clone();
                    }
                    let r = c.call("remove_fillers", a)?;
                    out(
                        format!(
                            "Removed {} filler word(s). Project is now {:.2}s.",
                            r["removed"],
                            r["duration"].as_f64().unwrap_or(0.0)
                        ),
                        r,
                    )
                }
                "show" => {
                    let r = c.call("show_transcript", json!({ "show": true }))?;
                    out("Transcript panel shown.".into(), r)
                }
                o => Err(format!("unknown action {o}")),
            }
        }
        _ => Err(format!("unknown tool {name}")),
    }
}
