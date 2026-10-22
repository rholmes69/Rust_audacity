<p align="center"><img src="assets/rust_audacity_logo.png" alt="Rust Audacity logo" width="320"></p>

# Rust Audacity

An Audacity-style audio editor written in Rust. It uses
[egui/eframe](https://github.com/emilk/egui) for the UI, [cpal](https://github.com/RustAudio/cpal) for audio I/O,
[Symphonia](https://github.com/pdeljanov/Symphonia) for decoding, [hound](https://github.com/ruuda/hound) for WAV writing,
[flacenc](https://github.com/yotarok/flacenc-rs), [LAME](https://lame.sourceforge.io) and [libvorbis/aoTuV](https://github.com/ComunidadAylas/vorbis-rs)
for FLAC/MP3/Ogg export, [realfft](https://github.com/HEnquist/realfft) for FFTs and [ebur128](https://github.com/sdroege/ebur128) for loudness.

## Build & run (Windows)

1. Install Rust from <https://rustup.rs> (pick the default MSVC toolchain).
2. In this folder:

   ```powershell
   cargo run --release
   ```

   You can also pass files to open: `cargo run --release -- song.mp3 voice.wav`.
   The finished program is `target\release\rust_audacity.exe`.

   To get a desktop shortcut with the app icon, double-click **`Create Desktop Shortcut.bat`**. It builds the app first if needed.

The MP3 and Ogg encoders are C libraries that Cargo compiles for you, so the first build takes a little longer.
The Visual Studio C++ build tools that come with the MSVC Rust toolchain are all you need.

On Linux you also need `libasound2-dev`, `libgtk-3-dev`, `libxkbcommon-dev` and `autoconf`/`make` (for LAME).

## Features

### Core editor

- **Projects:** File ▸ Save Project (Ctrl+S) / Open Project (Ctrl+O) saves everything in one `.rap` file: every track's audio (lossless 32-bit float), names, gain, pan, mute/solo, the selection and the zoom. Saving is crash-safe (it writes a temporary file, then swaps it in). You're asked to save before New, Open or closing with unsaved changes. Undo history isn't saved. Double-click or drop a `.rap` file to open it.
- **Import:** WAV, MP3, FLAC, OGG, M4A/AAC, AIFF and more. Use drag & drop or File ▸ Import.
- **Export (File ▸ Export Audio, Ctrl+Shift+E):**
  - WAV: 16-bit PCM (dithered), 24-bit PCM or 32-bit float.
  - FLAC: 16- or 24-bit, lossless.
  - MP3: 64–320 kbps constant bitrate.
  - Ogg Vorbis: quality 0–10.

  Export runs in the background and remembers your last format.
- **Effects run in the background.** A progress window with **Cancel** appears for anything that takes more than a moment, and the window stays responsive.
- **Time Shift tool (F5, ↔ button):**
  - Drag a track left or right to move it in time. It never cuts audio: moving left only removes leading silence.
  - Select some audio first, then drag inside the selection to move just that part. Drop it on another track to move it there.

  Each move is one undo step. F1 switches back to the Selection tool.
- **Playback:** play, pause, stop, loop, and click the timeline to play from that point.
- **Recording:** records from the microphone, with overdub.
- **Editing:** cut, copy, paste, delete, silence, trim, duplicate and split, with 100-step undo.
- **Tracks:** multiple tracks with gain, pan, mute and solo.

### Audio enhancement (20 features)

Every effect works on the selected tracks over the time selection, or on whole tracks if nothing is selected.

| # | Category | Feature | Where |
|---|---|---|---|
| 1 | Cleanup | **Noise Reduction**: learns a noise profile, then gates it out per frequency band | Effect ▸ Cleanup |
| 2 | Cleanup | **Click & Pop Removal**: finds clicks and rebuilds the waveform across them | Effect ▸ Cleanup |
| 3 | Cleanup | **Hum Removal**: 50/60 Hz notches plus harmonics | Effect ▸ Cleanup |
| 4 | Cleanup | **De-Esser**: split-band sibilance control | Effect ▸ Cleanup |
| 5 | Cleanup | **Clip Repair**: rebuilds flattened peaks | Effect ▸ Cleanup |
| 6 | Dynamics | **Compressor**: threshold, ratio, attack, release, soft knee, make-up | Effect ▸ Dynamics |
| 7 | Dynamics | **Limiter**: look-ahead brick-wall limiter that never goes over the ceiling | Effect ▸ Dynamics |
| 8 | Dynamics | **Noise Gate**: threshold, reduction, attack, hold, release | Effect ▸ Dynamics |
| 9 | Dynamics | **Loudness Normalize (LUFS)**: EBU R128, with −14/−16/−23 presets and an optional −1 dB limiter | Effect ▸ Dynamics |
| 10 | EQ & Tone | **Graphic EQ**: 10 bands, live response curve, 8 presets | Effect ▸ EQ & Tone |
| 11 | EQ & Tone | **Bass & Treble**: shelving tone controls | Effect ▸ EQ & Tone |
| 12 | EQ & Tone | **Distortion**: soft, hard, tube and fuzz | Effect ▸ EQ & Tone |
| 13 | Time & Pitch | **Change Pitch** without changing speed | Effect ▸ Time & Pitch |
| 14 | Time & Pitch | **Change Tempo** without changing pitch (WSOLA) | Effect ▸ Time & Pitch |
| 15 | Space | **Reverb**: Freeverb-style; the tail rings out over the audio that follows | Effect ▸ Space & Stereo |
| 16 | Space | **Stereo Widener**: mid/side width plus mid and side levels | Effect ▸ Space & Stereo |
| 17 | Separation | **Stem Separation**: vocals, drums, bass, other (and guitar/piano) | Stems ▸ Separate Stems… |
| 18 | Analysis | **Spectrogram View**: log-frequency heat map for each track | "Spectrum" button on each track |
| 19 | Analysis | **Plot Spectrum & Loudness Stats**: FFT plot with note read-out, LUFS, LRA, true peak, RMS, crest factor, DC offset | Analyze menu |
| 20 | Workflow | **Effect Preview & Effect Chains**: ▶ Preview in every dialog; chains run several effects as one undo step | Effect ▸ Effect Chains… |

Effect dialogs remember your last settings.

### Noise Reduction: two steps

1. Select a few seconds of noise only (room tone between words) and open Effect ▸ Cleanup ▸ Noise Reduction.
2. Click **Get Noise Profile**.
3. Select the audio to clean, or nothing to clean the whole track, and click **OK**.

### Stem separation

Use **Stems ▸ Separate Stems…** and pick an engine:

- **Demucs AI (recommended).** This runs Meta's open-source Hybrid Transformer Demucs model:
  - `htdemucs`: 4 stems (vocals, drums, bass, other).
  - `htdemucs_ft`: 4 stems, slower but better.
  - `htdemucs_6s`: 6 stems, adding guitar and piano.

  The "Vocals + Instrumental" option gives 2 stems. Each stem becomes a new track at the right time position, and the original track is muted.

  **One-time setup:** install Python 3, then run

  ```powershell
  pip install -U demucs
  ```

  The default command is `python -m demucs`. You can change it in the dialog, for example to a full path like `"C:\Python312\python.exe" -m demucs`. Demucs downloads its model (about 80 MB) the first time you use it. On a CPU it takes roughly as long as the song; an NVIDIA GPU is much faster. If Demucs reports an error, the dialog shows its last output lines.
- **Built-in.** This is instant and needs no install. It produces Vocals + Instrumental using spectral centre extraction: per frequency bin, sound that is identical in left and right within the vocal range goes to the vocal stem. It only works on stereo mixes with a centre-panned vocal.

**Stems ▸ Quick Vocal Remover** is the classic karaoke trick. It cancels the centre of the mix while keeping the bass.

### Built-in effect chains

| Chain | Steps |
|---|---|
| Podcast Voice | High-pass 80 Hz → Gate → De-ess → Compress 3:1 → −16 LUFS |
| Clean Up Voice Recording | Hum removal → Click removal → High-pass → Voice EQ → Normalize |
| Music Master (Streaming) | High-pass 25 Hz → Gentle 2:1 compression → −14 LUFS |
| Vinyl Restoration | Click removal → Hum removal → Rumble filter → Normalize |
| Karaoke | Vocal remover → −14 LUFS |
| Lo-Fi Radio | Band-limit 300–3400 Hz → Soft distortion → Normalize |

To change a built-in chain, click **Duplicate to edit**. You can reorder steps, change each step's settings and add steps. Your own chains are saved automatically.

### Transcription and text-based editing (Whisper)

**Analyze ▸ Transcribe Speech (Whisper)…** turns the selected track's speech into text with a time for every word. It uses OpenAI's open-source Whisper model running on your own PC. Nothing is uploaded.

**One-time setup:** install Python 3 from python.org, then run

```powershell
pip install -U faster-whisper
```

The model you pick downloads the first time you use it. Sizes run from `tiny` (75 MB, rough) to `large-v3` (3 GB, best); `small` is a good default. An NVIDIA GPU is used automatically when available. On a CPU, `small` takes roughly as long as the audio.

The **transcript panel** (View ▸ Transcript Panel, Ctrl+Shift+T) shows the text:

- **Select by text.** Click a word to select its audio, Shift+click to select a range of words, and double-click to play from a word. The current word lights up during playback.
- **Cut by text.** **✂ Delete words** cuts the selected words out of the audio. By default every track is cut, so music or a second microphone stays in sync; untick "Cuts apply to all tracks" to cut only the voice.
- **Remove fillers** cuts every "um", "uh", "erm" and similar word in one step.
- **Find words** highlights matches. Uncertain words are underlined and fillers are shown in orange.
- **Export ▸** saves subtitles (.srt) or plain text (.txt), or copies the text.

Words also appear along the bottom of the track's waveform. The transcript stays lined up through edits: cutting, pasting, trimming, time-shifting and tempo changes all move the words with the audio. It is part of undo and is saved in `.rap` projects.

### Let an AI agent edit with you (Agents menu)

While the app is open, AI agents connected through the Rust Audacity MCP server can work in this window using the `app_*` tools. They can open files, select, edit, apply effects, play, transcribe and cut words, export and save, and you watch it happen. Every change is a normal undo step.

- **Turning it off:** Agents ▸ "Allow AI agents to control this window" (on by default).
- **Status bar:** shows 🤖 while an agent is working.
- **Security:** the app only listens on your own computer (127.0.0.1) and uses a secret token stored in your temp folder, so only programs running as you can connect.

## Use it from AI agents (MCP server)

`rust_audacity_mcp` is a second program built from the same engine. It lets AI agents such as Claude Desktop, Claude Code and Cursor analyze, clean up, edit, convert, mix, stem-separate and transcribe audio files. It also lets them drive the open Rust Audacity window through the `app_*` tools.

```powershell
cargo build --release --bin rust_audacity_mcp
```

Setup and the full tool list are in [MCP.md](MCP.md).

To use a **local Ollama model** instead, run `rust_audacity_chat --model qwen3:8b --root <your audio folder>`. See [MCP.md › Use it with Ollama](MCP.md#use-it-with-ollama-local-models).

## Keyboard shortcuts

| Key | Action |
|---|---|
| Space / P / R / L | Play–Stop / Pause / Record / Loop |
| Ctrl+Z, Ctrl+Y | Undo, Redo |
| Ctrl+X / C / V, Delete | Cut / Copy / Paste, Delete |
| Ctrl+L / T / D / A | Silence / Trim / Duplicate / Select all |
| Ctrl+1 / Ctrl+3 / Ctrl+E / Ctrl+F | Zoom in / out / to selection / fit |
| Ctrl+N / Ctrl+O | New project / Open project |
| Ctrl+S / Ctrl+Shift+S | Save project / Save As |
| Ctrl+Shift+I / Ctrl+Shift+E | Import audio / Export audio |
| F1 / F5 | Selection tool / Time Shift tool |
| Ctrl+Shift+T | Show / hide the transcript panel |

## Code layout

```
src/
  lib.rs             audio engine library shared by the app and the MCP server
  bin/rust_audacity_mcp/  MCP server (main.rs = protocol, tools.rs = file tools, app_tools.rs = app_* tools)
  bin/rust_audacity_chat.rs  terminal chat that connects an Ollama model to the MCP server
  main.rs            window setup
  app.rs             main UI: menus, transport, tracks, waveform/spectrogram, dialogs
  app/fx_ui.rs       effect dialogs with Preview, Effect Chains editor
  app/analyze_ui.rs  spectrum plot and loudness statistics window
  app/stems_ui.rs    stem separation dialog and background job
  app/jobs_ui.rs     background effect/export jobs with progress and Cancel
  app/file_ui.rs     project Open/Save, "save changes?" prompt, export dialog
  app/transcript_ui.rs  Transcribe dialog, transcript panel, text-based editing
  app/bridge_ui.rs   runs agent commands in the window
  projectfile.rs     .rap project file format (save/load)
  transcript.rs      word timings that follow edits, fillers, SRT/TXT
  whisper.rs         runs Whisper (whisper_runner.py) and parses its output
  bridge.rs          localhost agent bridge (server in the app, client in the MCP server)
  effects.rs         effect catalogue (serialisable), render/apply/preview/chains
  ui_effects.rs      parameter editors for every effect
  spectrogram.rs     spectrogram rendering and texture cache
  dsp/
    stft.rs          FFT/STFT analysis and resynthesis (realfft)
    denoise.rs       noise-profile spectral gate
    restore.rs       click removal, hum notches, clip repair
    dynamics.rs      compressor, limiter, gate, de-esser
    tone.rs          graphic EQ, bass/treble, distortion
    biquad.rs        RBJ cookbook filters
    stretch.rs       WSOLA time-stretch, pitch shift
    space.rs         reverb, stereo width, vocal remover
    analysis.rs      LUFS/LRA/true-peak, spectrum
    separation.rs    built-in centre extraction, Demucs runner
  project.rs         tracks, selection, undo, edits, time shift
  fileio.rs          import (Symphonia) and WAV/FLAC/MP3/Ogg export
  track.rs, engine.rs, waveform.rs
```

Run `cargo test` to run the 64 unit tests. They cover every DSP block, for example: the limiter never exceeds its ceiling, noise reduction lowers noise while keeping a tone, pitch shift doubles the frequency without changing the length, separated stems sum back to the original, the Demucs command is checked with a stand-in script, projects save and reload bit-exactly, every export format decodes back correctly, background jobs commit as one undo step (or nothing if cancelled), time shifting never cuts audio, transcripts stay aligned through edits and undo, the Whisper pipeline works (with a stand-in model), and the agent bridge round-trips and rejects wrong tokens.

## Known limitations

- Each track is one continuous clip, so a moved region overwrites what is at the destination rather than creating separate overlapping clips.
- Progress for a single effect on a single track shows as "working…" rather than a percentage. Cancel takes effect immediately in the window; the worker finishes its current step in the background and the result is thrown away.
- Resampling uses linear interpolation.
- Whisper word times are accurate to roughly a tenth of a second. Cutting a word can occasionally clip the start of the next one; undo and select a little less if so.
- Transcription covers one track at a time.
- Not built yet: envelopes, editable labels, choosing audio devices, and metadata tags on export.

## Licences of bundled encoders

MP3 export uses LAME, which is LGPL. It is compiled into the program. That's fine for personal use; if you distribute the program, read the LGPL's terms for static linking. The FLAC encoder (flacenc) is Apache-2.0, and libvorbis/libogg are BSD.
