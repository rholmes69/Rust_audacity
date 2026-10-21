# Stand-in for the Demucs CLI (same args, progress output and folder layout) for UI testing.
import sys, os, time, wave, struct
args = sys.argv[1:]
model = args[args.index("-n") + 1]; out = args[args.index("-o") + 1]
two = any(a.startswith("--two-stems") for a in args); inp = args[-1]
for p in range(0, 101, 5):
    sys.stderr.write("\r%3d%%|%-20s| %d/100 [00:01<00:01]" % (p, "#" * (p // 5), p)); sys.stderr.flush(); time.sleep(0.25)
sys.stderr.write("\n")
import array
blob = open(inp, "rb").read()
ch = struct.unpack("<H", blob[22:24])[0]
i = blob.index(b"data"); size = struct.unpack("<I", blob[i+4:i+8])[0]
a = array.array('f', blob[i+8:i+8+size])  # 32-bit float samples
name = os.path.splitext(os.path.basename(inp))[0]
dest = os.path.join(out, model, name); os.makedirs(dest, exist_ok=True)
stems = ["vocals", "no_vocals"] if two else ["drums", "bass", "other", "vocals"]
gains = {"vocals": 0.6, "no_vocals": 0.4, "drums": 0.2, "bass": 0.5, "other": 0.3}
for s in stems:
    g = gains[s]
    b = array.array('h', [int(max(-1, min(1, x * g)) * 32767) for x in a])
    o = wave.open(os.path.join(dest, s + ".wav"), "wb"); o.setnchannels(ch); o.setsampwidth(2); o.setframerate(44100); o.writeframes(b.tobytes()); o.close()
