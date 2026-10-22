# Rust Audacity MCP server

`rust_audacity_mcp` lets AI agents (Claude Desktop, Claude Code, Cursor, VS Code, and any other
[Model Context Protocol](https://modelcontextprotocol.io) client) analyze and edit audio files using the same engine as
the Rust Audacity app. It runs locally and talks to the client over stdio.

## Build

```powershell
cd C:\Users\sales\Rust-Audacity
cargo build --release --bin rust_audacity_mcp
```

This produces `target\release\rust_audacity_mcp.exe`. Running `cargo build --release` builds both the app and the server.

## Connect it to an agent

`--root` restricts the server to one folder. It's recommended: the agent can then only read and write audio inside that folder.

### Claude Desktop

Edit `%APPDATA%\Claude\claude_desktop_config.json` (Settings ▸ Developer ▸ Edit Config), then restart Claude:

```json
{
  "mcpServers": {
    "rust-audacity": {
      "command": "C:\\Users\\sales\\Rust-Audacity\\target\\release\\rust_audacity_mcp.exe",
      "args": ["--root", "C:\\Users\\sales\\Music"]
    }
  }
}
```

### Claude Code

```powershell
claude mcp add rust-audacity --scope user -- "C:\Users\sales\Rust-Audacity\target\release\rust_audacity_mcp.exe" --root "C:\Users\sales\Music"
```

### Other clients (Cursor, VS Code, …)

Use the same command and args in the client's MCP settings. The transport is **stdio**.

### Options

| Flag | Meaning |
|---|---|
| `--root <folder>` | Only allow files inside this folder. Relative paths are resolved against it. |
| `--demucs-command "<cmd>"` | How to run Demucs for AI stem separation. Default `python -m demucs` on Windows, `demucs` elsewhere. |
| `--python <python>` | The Python that has `faster-whisper` installed, for `transcribe_audio`. Default `python` on Windows, `python3` elsewhere. |

## Tools

| Tool | What it does |
|---|---|
| `audio_info` | Duration, sample rate, channels, peak, true peak, RMS, LUFS. Reads WAV/MP3/FLAC/OGG/M4A/AIFF… |
| `analyze_audio` | Full diagnosis: loudness (LUFS/LRA/true peak), clipping, DC offset, noise floor, silent regions, frequency-band balance, dominant frequencies with note names, and mains-hum detection. Returns `issues` and `suggested_effects` that can be passed straight to `apply_effects`. |
| `list_effects` | All 27 effects with ids, categories, descriptions and default parameters, plus the built-in chains. |
| `apply_effects` | Apply a list of effects and/or a built-in chain (e.g. `"Podcast Voice"`) to a whole file or a time range, then write a new WAV. Reports loudness before and after. `noise_reduction` takes its profile from `noise_profile` or from the quietest 0.5 s automatically. |
| `edit_audio` | Operations: trim, delete, silence, insert_silence, fade_in, fade_out, gain, reverse, remove_silence (shortens long pauses). |
| `convert_audio` | Any format to WAV, FLAC, MP3 or Ogg Vorbis, with optional new sample rate, mono/stereo, bit depth and LUFS normalization. |
| `mix_audio` | Mix several files with per-input gain, pan, offset and mute, e.g. recombine stems or put music under a voice. |
| `generate_audio` | Tone (sine/square/saw/triangle), noise (white/pink/brown) or silence. |
| `separate_stems` | Split a song into stems as WAV files. `engine: "demucs"` gives 4 stems (vocals/drums/bass/other), 6 stems with `htdemucs_6s`, or 2 stems with `two_stems`; it needs `pip install -U demucs`, reports progress and can be cancelled. `engine: "builtin"` gives an instant vocals + instrumental split with nothing to install. |
| `render_spectrogram` | Saves a PNG spectrogram and returns it as an image the agent can look at. |
| `transcribe_audio` | Speech-to-text with Whisper. Returns text plus `[start, end, word]` timings, and can write `.srt`, `.txt` or `.json`. Needs `pip install -U faster-whisper`. |

### Tools for the open app window

These tools need the Rust Audacity app to be running (Agents ▸ "Allow AI agents to control this window" must be on). The agent works in the window you're looking at, and everything it does can be undone with Ctrl+Z.

| Tool | What it does |
|---|---|
| `app_status` | Tracks, selection, running job, undo history, transcript info; `measure: true` adds loudness. |
| `app_open` | Import an audio file as a new track, or open a `.rap` project. Waits for the import to finish. |
| `app_select` | Set the time selection (seconds) and which tracks are selected; optional zoom. |
| `app_edit` | cut, copy, paste, delete, silence, trim, duplicate, split_new, mix_and_render, select_all, remove_selected_tracks, undo, redo. |
| `app_apply_effects` | Effects and/or a chain on the selection as one undo step. Waits, then reports loudness. |
| `app_track` | Name, gain, pan, mute, solo, selected. |
| `app_playback` | Play (from/to) or stop, through your speakers. |
| `app_export` | Export the mix or the selection; the format comes from the extension. |
| `app_save_project` | Save as `.rap`. |
| `app_transcript` | `transcribe` (Whisper), `get` (words with indices and times), `delete_words` (cut words by index), `remove_fillers`, `show`. |

Inputs are never modified: results go to a new file (default `<name>_processed.wav` and similar). Writing over the input requires `overwrite: true`.

**Output formats:** every tool that writes audio picks the format from the output file's extension:

| Extension | Format | Options |
|---|---|---|
| `.wav` | WAV | `format`: `pcm16`, `pcm24` (default) or `float32` |
| `.flac` | FLAC, lossless | `format`: `pcm16` gives 16-bit, anything else 24-bit |
| `.mp3` | MP3, constant bitrate | `mp3_kbps`: 64–320 (default 192) |
| `.ogg` | Ogg Vorbis | `ogg_quality`: 0–10 (default 6) |

`separate_stems` takes `file_type` (`wav`, `flac`, `mp3` or `ogg`).
Every result contains a readable summary plus machine-readable `structuredContent`.

### Example `apply_effects` call

```json
{
  "input": "interview.mp3",
  "output": "interview_clean.wav",
  "effects": [
    "hum_removal",
    { "effect": "noise_reduction", "params": { "reduction_db": 15 } },
    { "effect": "compressor", "params": { "threshold_db": -20, "ratio": 3 } },
    { "effect": "loudness_normalize", "params": { "target_lufs": -16 } }
  ]
}
```

## Use it with Ollama (local models)

Ollama runs models on your PC but is not an MCP client by itself, so the project includes a small bridge, **`rust_audacity_chat`**. It starts `rust_audacity_mcp` for you, passes its tools to an Ollama model, runs the tool calls the model makes and shows the results.

```
you ⇄ rust_audacity_chat ⇄ Ollama (localhost:11434, /api/chat with tools)
              ⇵
     rust_audacity_mcp (MCP over stdio)
```

### Setup (Windows)

1. Install Ollama from <https://ollama.com> and pull a model that supports **tool calling**:

   ```powershell
   ollama pull qwen3:8b
   ```

   Good choices are `qwen3:8b` (the default), `qwen3:14b`, `llama3.1:8b` and `qwen2.5:7b`. Models smaller than about 7B often call tools badly.
2. Build both programs:

   ```powershell
   cd C:\Users\sales\Rust-Audacity
   cargo build --release --bin rust_audacity_mcp --bin rust_audacity_chat
   ```
3. Chat, pointing `--root` at your audio folder:

   ```powershell
   .\target\release\rust_audacity_chat.exe --model qwen3:8b --root C:\Users\sales\Music
   ```

   You can also double-click `mcp\chat_with_ollama.bat`, which uses your Music folder.

You'll see each tool call and its result:

```
you> fix interview.mp3 for a podcast
🔧 analyze_audio {"path":"interview.mp3"}
   ✓ … Issues: mains hum at 60 Hz; audible background noise; quiet: -22.5 LUFS
🔧 apply_effects {"input":"interview.mp3","output":"interview_fixed.wav","effects":[…]}
   ✓ Wrote C:\Users\sales\Music\interview_fixed.wav
assistant> Removed the hum and background noise and raised it to -16 LUFS → interview_fixed.wav
```

Commands: `/files` lists the audio in the folder, `/tools` lists the tools, `/reset` clears the conversation and `/exit` quits.
For one request without the chat, use `--prompt "…"`.

### Options

| Option | Default | Meaning |
|---|---|---|
| `--model` | `qwen3:8b` | Any installed Ollama model that supports tools. |
| `--root` | current folder | The folder the tools may read and write. |
| `--ollama` | `http://localhost:11434` or `$OLLAMA_HOST` | Ollama address, e.g. another PC on your network. |
| `--ctx` | 16384 | Context window requested from Ollama. The tool list alone is a few thousand tokens. |
| `--think` | off | Let reasoning models think before answering. Smarter but slower. |
| `--vision` | off | Send spectrogram images to the model. Use a vision model such as `qwen2.5vl` or `llama3.2-vision`. |
| `--server-arg` | – | Pass an extra argument to the MCP server, e.g. `--server-arg --demucs-command --server-arg "py -m demucs"`. |
| `--max-steps` | 12 | Maximum tool calls per request. |
| `--verbose` / `--no-color` | – | Show full tool output / plain text output. |

### Tips for local models

- Name the file and the goal, e.g. "analyze song.mp3, then make it -14 LUFS". Smaller models do better with direct requests.
- If the model ignores tools or invents file names, try a larger model (`qwen3:14b`) or `--think`.
- If you get "does not support tools", that model can't call tools. Pick another one.
- Stem separation with Demucs runs on your CPU or GPU next to the model. The quick `builtin` engine is instant.
- Other MCP clients that support Ollama can also use `rust_audacity_mcp` directly: configure them with the command `rust_audacity_mcp.exe` and the arguments `--root <folder>`.

### Testing without a model

`mcp/mock_ollama.py` imitates Ollama's `/api/chat` tool-calling responses. Start it with `python mcp/mock_ollama.py 11500`, then run `rust_audacity_chat --ollama http://127.0.0.1:11500 --root <folder with problem.wav and song.wav> -p "please fix problem.wav"`.
The bridge was tested this way end to end: analyze → apply suggested effects → verify. The tests also cover progress display, string-encoded tool arguments, tool errors, a missing model and Ollama not running. It has **not** been run against a real Ollama model yet, because Ollama couldn't be downloaded in the build environment.

## Things to ask an agent

- "Analyze `C:\Users\sales\Music\interview.mp3` and fix whatever is wrong with it for a podcast."
- "Split `song.mp3` into stems and make a karaoke version without vocals."
- "Shorten all the long pauses in `lecture.wav` and normalize it to −16 LUFS."
- "Show me the spectrogram of the first 10 seconds of `noisy.wav`. Is there hum?"
- "Make `track.flac` 48 kHz, 16-bit and −14 LUFS for streaming."
- "Convert every WAV in this folder to 320 kbps MP3."
- With the app open: "Transcribe the interview I have open, cut the off-topic part about the weather and all the ums, then make it −16 LUFS and export interview.mp3."
- "Make subtitles for talk.mp4's audio" (`transcribe_audio` with `output: "talk.srt"`).

## Protocol details

- MCP over stdio: newline-delimited JSON-RPC 2.0. Logs go to stderr.
- Protocol versions 2025-11-25, 2025-06-18, 2025-03-26 and 2024-11-05 are negotiated.
- Implements `initialize`, `ping`, `tools/list`, `tools/call`, `notifications/progress` (when the client sends a `progressToken`) and `notifications/cancelled` (stops a running Demucs job). `resources/list` and `prompts/list` return empty lists.
- Each tool call runs on its own thread, so a long stem separation doesn't block other calls.
- The `app_*` tools reach the app over the agent bridge: a localhost TCP socket (`127.0.0.1`, random port) that takes one JSON request per line and needs a secret token. The app writes the port and token to `rust_audacity_bridge.json` in your temp folder while it runs.

## Testing

`cargo test` covers the protocol helpers and effect-spec parsing. The server was also tested end-to-end with the official MCP Python SDK client:

- every tool and all 27 effects
- progress notifications and cancellation
- the `--root` sandbox and error handling

The real Demucs model could not be downloaded in the test environment. The Demucs path was therefore tested with a stand-in that mimics the Demucs command line.
