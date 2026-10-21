"""Mock of Ollama's /api/tags and /api/chat (tool calling) for testing rust_audacity_chat.
Scripted 'model': analyze -> apply suggested effects (args as JSON string, like some models emit) -> verify -> answer."""
import json, re, sys
from http.server import BaseHTTPRequestHandler, HTTPServer

LOG = []
class H(BaseHTTPRequestHandler):
    def log_message(self, *a): pass
    def reply(self, obj, code=200):
        b = json.dumps(obj).encode()
        self.send_response(code); self.send_header("Content-Type", "application/json"); self.send_header("Content-Length", str(len(b))); self.end_headers(); self.wfile.write(b)
    def do_GET(self):
        if self.path == "/api/tags":
            self.reply({"models": [{"name": "qwen3:8b"}, {"name": "llama3.1:8b"}]})
        else:
            self.reply({"error": "not found"}, 404)
    def do_POST(self):
        req = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
        LOG.append(req)
        assert self.path == "/api/chat"
        if req["model"] not in ("qwen3:8b", "llama3.1:8b"):
            return self.reply({"error": f"model '{req['model']}' not found"}, 404)
        tools = {t["function"]["name"] for t in req["tools"]}
        assert req["tools"][0]["type"] == "function" and "parameters" in req["tools"][0]["function"]
        assert req["stream"] is False and req["options"]["num_ctx"] > 0
        msgs = req["messages"]
        assert msgs[0]["role"] == "system" and "problem.wav" in msgs[0]["content"], "system prompt should list files"
        tool_msgs = [m for m in msgs if m["role"] == "tool"]
        for m in tool_msgs:
            assert m.get("tool_name") in tools
        user = [m for m in msgs if m["role"] == "user"][-1]["content"]
        n = len([m for m in msgs[msgs.index([m for m in msgs if m["role"]=="user"][-1]):] if m["role"] == "tool"])
        def call(name, args):
            return self.reply({"model": req["model"], "message": {"role": "assistant", "content": "", "tool_calls": [{"function": {"name": name, "arguments": args}}]}, "done": True})
        if "fix" in user:
            if n == 0:
                return call("analyze_audio", {"path": "problem.wav"})
            if n == 1:
                sug = re.search(r"Suggested effects: (\[.*\])", tool_msgs[-1]["content"]).group(1)
                # Emit arguments as a JSON *string* to exercise normalization.
                return call("apply_effects", json.dumps({"input": "problem.wav", "output": "problem_fixed.wav", "effects": json.loads(sug)}))
            if n == 2:
                return call("audio_info", {"path": "problem_fixed.wav"})
            return self.reply({"model": req["model"], "message": {"role": "assistant", "content": "Done. Removed the 60 Hz hum and background noise and normalized to -16 LUFS. Output: problem_fixed.wav\n[last tool said: " + tool_msgs[-1]["content"][:120] + "]"}, "done": True})
        if "stems" in user:
            if n == 0:
                return call("separate_stems", {"input": "song.wav", "engine": "builtin"})
            return self.reply({"model": req["model"], "message": {"role": "assistant", "content": "Stems written: " + tool_msgs[-1]["content"].splitlines()[1]}, "done": True})
        if "bad" in user:
            if n == 0:
                return call("apply_effects", {"input": "song.wav", "effects": ["autotune"]})
            return self.reply({"model": req["model"], "message": {"role": "assistant", "content": "That effect does not exist; error was relayed: " + tool_msgs[-1]["content"][:60]}, "done": True})
        return self.reply({"model": req["model"], "message": {"role": "assistant", "content": "Hello! Ask me to process audio."}, "done": True})

HTTPServer(("127.0.0.1", int(sys.argv[1])), H).serve_forever()
