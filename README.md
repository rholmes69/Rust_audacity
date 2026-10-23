<p align="center"><img src="assets/rust_audacity_logo.png" alt="Rust Audacity logo" width="320"></p>

# Rust Audacity

[![CI](https://github.com/rholmes69/Rust_audacity/actions/workflows/ci.yml/badge.svg)](https://github.com/rholmes69/Rust_audacity/actions/workflows/ci.yml)

An Audacity-style audio editor written in Rust. It uses
[egui/eframe](https://github.com/emilk/egui) for the UI, [cpal](https://github.com/RustAudio/cpal) for audio I/O,
[Symphonia](https://github.com/pdeljanov/Symphonia) for decoding, [hound](https://github.com/ruuda/hound) for WAV writing,
[flacenc](https://github.com/yotarok/flacenc-rs), [LAME](https://lame.sourceforge.io) and [libvorbis/aoTuV](https://github.com/ComunidadAylas/vorbis-rs)
for FLAC/MP3/Ogg export, [realfft](https://github.com/HEnquist/realfft) for FFTs and [ebur128](https://github.com/sdroege/ebur128) for loudness.

## Download (Windows)

Get the latest zip from the [Releases page](https://github.com/rholmes69/Rust_audacity/releases), unzip it anywhere and run `rust_audacity.exe`. To get a desktop icon, double-click `Create Desktop Shortcut.bat`. No Rust install is needed.

Every push to `main` is built and tested on Windows and Linux by GitHub Actions. The Windows `.exe` files from each run can be downloaded from that run's page under **Artifacts**.

**Publishing a release:** tag a version and push the tag. GitHub builds, tests, packages and publishes it automatically.

```powershell
git tag v0.5.0
git push origin v0.5.0
```

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

  Export runs in the background and remembers your last format. Title, artist, album, cover art and chapters (from labels) are written into MP3, FLAC, Ogg and WAV files; untick "Tags" in the Export dialog to leave them out.
- **Effects run in the background.** A progress window with **Cancel** appears for anything that takes more than a moment, and the window stays responsive.
- **Time Shift tool (F5, ↔ button):**
  - Drag a track left or right to move it in time. It never cuts audio: moving left only removes leading silence.
  - Select some audio first, then drag inside the selection to move just that part. Drop it on another track to move it there.

  Each move is one undo step. F1 switches back to the Selection tool.
- **Playback:** play, pause, stop, loop, and click the timeline to play from that point.
- **Recording:** records from the microphone, with overdub.
- **Editing:** cut, copy, paste, delete, silence, trim, duplicate and split, with 100-step undo.
- **Tracks:** multiple tracks with gain, pan, mute and solo.
- **Audio devices (Edit ▸ Preferences, Ctrl+P):** choose the playback and recording device, mono or stereo recording and the buffer size (lower = less latency). **Monitor input** shows your microphone level without recording so you can set your gain, and **Test playback** plays a short tone. If a saved device is unplugged, the system default is used and you're told.

### Labels and chapters

- **Add a label:** Ctrl+B (or Edit ▸ Add Label at Selection) labels the cursor position or the selected region, and lets you type its name. Labels appear in a strip under the ruler.
- **Edit labels:** drag a label to move it, drag a region's edges to resize it, click it to select its audio, double-click to rename. Right-click to play or delete it. Labels move with cuts, pastes and effects that change the length, and undo with everything else.
- **Labels window (Ctrl+Shift+B):** a table of all labels, with import and export of Audacity label files (`.txt`).
- **Export audio per label:** writes one file per label (a point label runs to the next label), optionally numbered and with the part before the first label as "Start". Each file gets the label as its title and a track number.
- **Chapters:** when you export, each label becomes a chapter (ID3 CHAP for MP3, Vorbis CHAPTER comments for FLAC/Ogg), so podcast players can skip between sections.

### Metadata (File ▸ Metadata…)

Set the title, artist, album, album artist, year, genre, track number, comment and cover art (JPEG or PNG). They're saved with the project and written into every export.

### Batch processing (File ▸ Batch Process…)

Run the same effect chain over many files at once, for example to clean up and level a whole season of podcast episodes:

1. Add files, or a whole folder (optionally with sub-folders).
2. Pick a chain (or "Nothing" to just convert), the output format and where to save. Each output is named after its input plus a suffix such as `_processed`.
3. Click **Process**. It runs in the background, so you can keep editing; the results list shows each file's loudness afterwards, and any failures.

Noise reduction takes its noise profile from each file's quietest half-second. Existing files are never overwritten unless you tick the option, and inputs are never touched.

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

### Volume envelopes (Envelope tool, F2)

Press **F2**, or click the **⌇** toolbar button, to draw volume changes on any track. Envelopes are written as points on an orange line; the 0, −12 and −24 dB lines are shown as a guide.

- **Add a point:** click anywhere on the track.
- **Move a point:** drag it. The status bar shows the level and time. Levels snap to 0 dB when you get close.
- **Delete a point:** right-click it (or Alt+click).
- **Remove the whole envelope:** Tracks ▸ Clear Envelope.

Envelopes are applied during playback and export without changing the recorded audio. The points move with the audio when you cut, paste, trim, time-shift or change tempo. They are part of undo and are saved in `.rap` projects.

### Auto Duck (Tracks ▸ Auto Duck…)

Auto Duck lowers music automatically whenever someone speaks:

1. Pick the voice track. The app guesses it: the transcribed track, or else the track with the most pauses.
2. Pick the tracks to lower, and how much (12 dB by default).
3. Click **Duck**.

The fades are written as envelope points, so you can see them and fine-tune any of them with the Envelope tool. If the voice track has a transcript, its word timings are used to find the speech, which is the most accurate method. Otherwise the voice level is used, with an adjustable threshold. You can also set:

- the fade-down and fade-up times;
- "start early", which begins the dip just before the first word;
- "stay down through pauses", which keeps the music from bouncing between sentences.

### Let an AI agent edit with you (Agents menu)

While the app is open, AI agents connected through the Rust Audacity MCP server can work in this window using the `app_*` tools. They can open files, select, edit, apply effects, play, transcribe and cut words, export and save, and you watch it happen. Every change is a normal undo step.

- **Turning it off:** Agents ▸ "Allow AI agents to control this window" (on by default).
- **Status bar:** shows 🤖 while an agent is working.
- **Security:** the app only listens on your own computer (127.0.0.1) and uses a secret token stored in your temp folder, so only programs running as you can connect.

## Use it from AI agents (MCP server)

Rust Audacity has a Model Context Protocol server with 25 tools. AI agents can use it to analyze, clean up, edit, convert, mix, stem-separate and transcribe audio files. With the `app_*` tools they can also drive the open window. The server comes in two forms, with exactly the same tools:

- **Built into the app (HTTP).** It runs whenever Rust Audacity is open, at `http://127.0.0.1:8765/mcp`.
  - **Agents ▸ MCP Server Settings & Setup…** shows the address and the access token, and lets you limit agents to one folder.
  - The same window has ready-to-copy setup for Claude Code, Cursor/Windsurf, VS Code and Claude Desktop.
  - The status bar shows `MCP ●`; it lights up and names the client and tool while an agent is working.
- **`rust_audacity_mcp.exe` (stdio).** This is for clients that start the server themselves, such as Claude Desktop and `rust_audacity_chat`. Build it with:

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
| F1 / F2 / F5 | Selection / Envelope / Time Shift tool |
| Ctrl+Shift+T | Show / hide the transcript panel |
| Ctrl+B / Ctrl+Shift+B | Add label / Labels window |
| Ctrl+P | Preferences (audio devices) |

## Code layout

```
src/
  lib.rs             audio engine library shared by the app and the MCP server
  bin/rust_audacity_mcp/  stdio MCP server (a thin wrapper around src/mcp)
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
  app/mcp_ui.rs      built-in MCP server: start/stop, settings & setup window, status
  mcp/               MCP protocol + all tools, shared by the app (HTTP) and rust_audacity_mcp (stdio)
    mod.rs           JSON-RPC handling, progress, cancellation
    http.rs          Streamable HTTP transport (localhost, token, Host/Origin checks)
    tools.rs         file tools; app_tools.rs = app_* tools
  app/envelope_ui.rs Envelope tool, envelope drawing, Auto Duck dialog
  app/prefs_ui.rs    Preferences: audio devices, buffer size, input monitor
  app/labels_ui.rs   label strip, Labels window, export per label
  app/meta_ui.rs     Metadata window (tags, cover art)
  app/batch_ui.rs    Batch Process window
  labels.rs          labels that follow edits, export regions, chapters, Audacity .txt
  metadata.rs        tags, cover art and chapters for MP3 (ID3), FLAC, Ogg and WAV
  batch.rs           batch engine shared by the app and the batch_process MCP tool
  projectfile.rs     .rap project file format (save/load)
  envelope.rs        volume envelope points (interpolation, follow edits)
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
    duck.rs          speech detection and ducking envelopes
  project.rs         tracks, selection, undo, edits, time shift
  fileio.rs          import (Symphonia) and WAV/FLAC/MP3/Ogg export
  engine.rs          playback/recording with device selection
  track.rs, waveform.rs
.github/workflows/   CI (build/test/clippy on Windows + Linux) and tag-triggered releases
```

Run `cargo test` to run the 77 unit tests. They cover every DSP block, for example: the limiter never exceeds its ceiling, noise reduction lowers noise while keeping a tone, pitch shift doubles the frequency without changing the length, separated stems sum back to the original, the Demucs command is checked with a stand-in script, projects save and reload bit-exactly, every export format decodes back correctly, background jobs commit as one undo step (or nothing if cancelled), time shifting never cuts audio, transcripts stay aligned through edits and undo, the Whisper pipeline works (with a stand-in model), the agent bridge round-trips and rejects wrong tokens, envelopes follow edits, Auto Duck lowers music only while speech is present, the HTTP MCP server enforces its token and rejects requests from web pages, labels follow edits and undo, tags/cover/chapters read back correctly from all four export formats, and batch processing levels every file.

## Known limitations

- Each track is one continuous clip, so a moved region overwrites what is at the destination rather than creating separate overlapping clips.
- Progress for a single effect on a single track shows as "working…" rather than a percentage. Cancel takes effect immediately in the window; the worker finishes its current step in the background and the result is thrown away.
- Resampling uses linear interpolation.
- Whisper word times are accurate to roughly a tenth of a second. Cutting a word can occasionally clip the start of the next one; undo and select a little less if so.
- Transcription covers one track at a time.
- The waveform is drawn without the envelope applied; the orange line shows the level.
- Envelope changes made while audio is playing are heard from the next time you press Play.
- Not built yet: editable labels, choosing audio devices, and metadata tags on export.

## Licences of bundled encoders

MP3 export uses LAME, which is LGPL. It is compiled into the program. That's fine for personal use; if you distribute the program, read the LGPL's terms for static linking. The FLAC encoder (flacenc) is Apache-2.0, and libvorbis/libogg are BSD.
