"""End-to-end test of rust_audacity_mcp using the official MCP Python SDK client (pip install mcp).

Usage:  python test_client.py <path to rust_audacity_mcp(.exe)> <empty work folder containing song.wav (stereo)> [fake_demucs.py]
If the third argument is omitted, the real Demucs is used via the server's default command.
"""
import asyncio, json, sys, os, base64
from mcp import ClientSession, StdioServerParameters
from mcp.client.stdio import stdio_client

BIN = os.path.abspath(sys.argv[1])
D = os.path.abspath(sys.argv[2])
FAKE = os.path.abspath(sys.argv[3]) if len(sys.argv) > 3 else None

def show(name, res, full=False):
    txt = res.content[0].text
    print(f"\n=== {name}  is_error={res.is_error}")
    print(txt if full else txt[:700])
    return res

async def main():
    args = ["--root", D] + (["--demucs-command", f"{sys.executable} {FAKE}"] if FAKE else [])
    params = StdioServerParameters(command=BIN, args=args)
    async with stdio_client(params) as (r, w):
        async with ClientSession(r, w) as s:
            init = await s.initialize()
            print("server:", init.server_info.name, init.server_info.version, "protocol", init.protocol_version)
            tools = await s.list_tools()
            print("tools:", [t.name for t in tools.tools])
            assert len(tools.tools) == 10

            # Build a problem file: voice-like tone + 60 Hz hum + noise, too quiet.
            assert not (await s.call_tool("generate_audio", {"output": "tone.wav", "kind": "tone", "duration": 6, "frequency": 330, "amplitude": 0.08, "channels": 2})).is_error
            assert not (await s.call_tool("generate_audio", {"output": "hum.wav", "kind": "tone", "duration": 6, "frequency": 60, "amplitude": 0.03, "channels": 2})).is_error
            assert not (await s.call_tool("generate_audio", {"output": "noise.wav", "kind": "noise", "noise_type": "pink", "duration": 6, "amplitude": 0.02})).is_error
            show("mix_audio", await s.call_tool("mix_audio", {"inputs": [{"path": "tone.wav", "offset": 1.0}, {"path": "hum.wav"}, {"path": "noise.wav"}], "output": "problem.wav"}))
            show("audio_info", await s.call_tool("audio_info", {"path": "problem.wav"}))
            an = show("analyze_audio", await s.call_tool("analyze_audio", {"path": "problem.wav"}))
            sc = an.structured_content
            print("issues:", sc["issues"])
            print("suggested:", sc["suggested_effects"])
            assert any("hum" in i for i in sc["issues"]), "hum not detected"
            # Fix it with the suggested effects.
            fx = show("apply_effects (suggested)", await s.call_tool("apply_effects", {"input": "problem.wav", "output": "fixed.wav", "effects": sc["suggested_effects"]}), full=True)
            assert not fx.is_error
            an2 = (await s.call_tool("analyze_audio", {"path": "fixed.wav"})).structured_content
            print("after issues:", an2["issues"], "lufs:", an2["levels"]["integrated_lufs"], "hum60:", an2["hum_prominence_db"])

            le = show("list_effects", await s.call_tool("list_effects", {}))
            assert len(le.structured_content["effects"]) == 27

            # Chains, ranges, every effect id.
            show("apply chain", await s.call_tool("apply_effects", {"input": "song.wav", "output": "podcast.wav", "chain": "podcast"}))
            for e in le.structured_content["effects"]:
                res = await s.call_tool("apply_effects", {"input": "song.wav", "output": f"fx_{e['id']}.wav", "effects": [e["id"]], "start": 1, "end": 3})
                assert not res.is_error, (e["id"], res.content[0].text)
            print("\nall 27 effects applied OK via MCP")
            show("pitch +5", await s.call_tool("apply_effects", {"input": "song.wav", "effects": [{"effect": "change_pitch", "params": {"semitones": 5}}]}))
            show("bad param", await s.call_tool("apply_effects", {"input": "song.wav", "effects": [{"effect": "compressor", "params": {"ratioo": 4}}]}))
            show("bad effect", await s.call_tool("apply_effects", {"input": "song.wav", "effects": ["autotune"]}))

            show("edit_audio", await s.call_tool("edit_audio", {"input": "fixed.wav", "output": "edited.wav", "operations": [
                {"op": "remove_silence", "min_duration": 0.5, "keep": 0.2}, {"op": "fade_in", "duration": 0.5}, {"op": "fade_out", "duration": 1},
                {"op": "insert_silence", "at": 0, "duration": 0.25}, {"op": "gain", "db": -3}]}), full=True)
            show("convert", await s.call_tool("convert_audio", {"input": "song.wav", "output": "song_48k_mono.wav", "sample_rate": 48000, "channels": 1, "normalize_lufs": -14, "format": "pcm16"}), full=True)

            # Stems: builtin, then "demucs" (stand-in script) with progress notifications.
            show("stems builtin", await s.call_tool("separate_stems", {"input": "song.wav", "engine": "builtin"}), full=True)
            prog = []
            async def on_progress(p, total, msg):
                prog.append((p, total, msg))
            st = show("stems demucs", await s.call_tool("separate_stems", {"input": "song.wav", "engine": "demucs", "output_dir": "stems_ai"}, progress_callback=on_progress), full=True)
            print("progress notifications:", len(prog), prog[:2], prog[-1:] if prog else None)
            assert not st.is_error and len(st.structured_content["stems"]) == 4 and prog
            # Recombine stems without vocals = karaoke.
            stems = {x["stem"]: x["path"] for x in st.structured_content["stems"]}
            show("remix", await s.call_tool("mix_audio", {"inputs": [{"path": p, "mute": n == "vocals"} for n, p in stems.items()], "output": "karaoke_mix.wav", "normalize_peak_db": -1}))

            sp = show("spectrogram", await s.call_tool("render_spectrogram", {"input": "song.wav", "width": 800, "height": 300}))
            img = [c for c in sp.content if c.type == "image"][0]
            open(os.path.join(D, "spec_from_mcp.png"), "wb").write(base64.b64decode(img.data))
            print("image bytes:", len(base64.b64decode(img.data)), img.mime_type)

            # Sandbox + errors.
            show("outside root", await s.call_tool("audio_info", {"path": "/etc/passwd"}))
            show("missing file", await s.call_tool("audio_info", {"path": "nope.wav"}))
            show("overwrite guard", await s.call_tool("apply_effects", {"input": "song.wav", "output": "song.wav", "effects": ["invert"]}))
            try:
                await s.call_tool("does_not_exist", {})
            except Exception as e:
                print("\nunknown tool ->", type(e).__name__, e)
            print("\nping:", await s.send_ping())
            print("\nALL CHECKS PASSED")

asyncio.run(main())
