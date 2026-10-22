# Rust Audacity Whisper runner.
#
# Started by Rust Audacity with:  python -u rust_audacity_whisper.py --audio <file> --model <name> [--language xx]
# <file> holds raw little-endian float32 samples, mono, 16 kHz.
# Prints one JSON object per line on stdout:
#   {"type":"status","message":...}  {"type":"info","engine":...,"language":...,"duration":...}
#   {"type":"progress","value":0..1}  {"type":"words","words":[[start,end,text,prob],...]}
#   {"type":"error","message":...}    {"type":"done"}
import argparse
import json
import os
import sys


def emit(**msg):
    sys.stdout.write(json.dumps(msg, ensure_ascii=False) + "\n")
    sys.stdout.flush()


def load_error(name, e):
    return (
        "Could not load the Whisper model '%s': %s\n"
        "The first time a model is used it is downloaded from the internet "
        "(huggingface.co), so check your connection or firewall." % (name, str(e).splitlines()[0][:200] if str(e) else type(e).__name__)
    )


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--audio", required=True)
    ap.add_argument("--model", default="base")
    ap.add_argument("--language", default=None)
    a = ap.parse_args()

    try:
        import numpy as np
    except ImportError:
        emit(type="error", message="numpy is missing. Run:  pip install -U faster-whisper")
        return 3
    audio = np.fromfile(a.audio, dtype="<f4").astype(np.float32)
    duration = max(len(audio) / 16000.0, 1e-6)
    engines = os.environ.get("RUST_AUDACITY_WHISPER_ENGINES", "faster,openai").split(",")

    if "faster" in engines:
        try:
            from faster_whisper import WhisperModel
        except ImportError:
            WhisperModel = None
        if WhisperModel is not None:
            emit(type="status", message="Loading Whisper model '%s' (downloads on first use)…" % a.model)
            try:
                model = WhisperModel(a.model, device="auto", compute_type="default")
            except Exception as e:
                # Usually missing CUDA libraries: try the CPU before giving up.
                try:
                    model = WhisperModel(a.model, device="cpu", compute_type="int8")
                except Exception as e2:
                    emit(type="error", message=load_error(a.model, e2 if str(e2) else e))
                    return 4
            segments, info = model.transcribe(
                audio, language=a.language, word_timestamps=True, vad_filter=False
            )
            emit(type="info", engine="faster-whisper", language=info.language, duration=duration)
            for seg in segments:
                words = [
                    [float(w.start), float(w.end), w.word, float(getattr(w, "probability", 1.0))]
                    for w in (seg.words or [])
                ]
                emit(type="words", words=words)
                emit(type="progress", value=min(1.0, float(seg.end) / duration))
            emit(type="progress", value=1.0)
            emit(type="done")
            return 0

    if "openai" in engines:
        try:
            import whisper
        except ImportError:
            whisper = None
        if whisper is not None:
            emit(type="status", message="Loading Whisper model '%s' (downloads on first use)…" % a.model)
            try:
                model = whisper.load_model(a.model)
            except Exception as e:
                emit(type="error", message=load_error(a.model, e))
                return 4
            emit(type="info", engine="openai-whisper", language=a.language or "auto", duration=duration)
            result = model.transcribe(
                audio, language=a.language, word_timestamps=True, verbose=False
            )
            words = []
            for seg in result.get("segments", []):
                for w in seg.get("words", []):
                    words.append([float(w["start"]), float(w["end"]), w["word"], float(w.get("probability", 1.0))])
            emit(type="info", engine="openai-whisper", language=result.get("language", ""), duration=duration)
            emit(type="words", words=words)
            emit(type="progress", value=1.0)
            emit(type="done")
            return 0

    emit(
        type="error",
        message="Whisper is not installed for this Python.\nRun:  pip install -U faster-whisper\n"
        "(or set the Python box to the Python that has it)",
    )
    return 3


if __name__ == "__main__":
    sys.exit(main())
