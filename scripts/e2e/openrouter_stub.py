#!/usr/bin/env python3
"""Loopback OpenRouter stub. Scenario from argv[2]; logs each request's path and
non-secret payload fields to argv[3] (JSON lines)."""
import json, sys, time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
port, scenario, log = int(sys.argv[1]), sys.argv[2], sys.argv[3]
class H(BaseHTTPRequestHandler):
    def log_message(self, *a): pass
    def reply(self, code, obj):
        b = json.dumps(obj).encode()
        self.send_response(code); self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(b))); self.end_headers(); self.wfile.write(b)
    def do_POST(self):
        body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
        rec = {"path": self.path, "model": body.get("model"), "provider": body.get("provider"),
               "keyterms": body.get("keyterms"), "language": body.get("language"),
               "reasoning": body.get("reasoning"), "max_tokens": body.get("max_tokens"),
               "has_audio": "input_audio" in body, "auth_bearer": self.headers.get("Authorization", "").startswith("Bearer ")}
        with open(log, "a") as f: f.write(json.dumps(rec) + "\n")
        if self.path.endswith("/audio/transcriptions"):
            if scenario == "stt500": return self.reply(500, {"error": {"message": "upstream"}})
            if scenario == "sttempty": return self.reply(200, {"text": "  "})
            return self.reply(200, {"text": "send it to ana on monday wait no actually tuesday"})
        if scenario == "llm500": return self.reply(500, {"error": {"message": "x"}})
        if scenario == "llmempty": return self.reply(200, {"choices": [{"finish_reason": "stop", "message": {"content": ""}}]})
        if scenario == "llmlength": return self.reply(200, {"choices": [{"finish_reason": "length", "message": {"content": "Send it to"}}]})
        if scenario == "llmslow": time.sleep(6)
        return self.reply(200, {"choices": [{"finish_reason": "stop", "message": {"content": "Send it to Ana on Tuesday."}}]})
ThreadingHTTPServer(("127.0.0.1", port), H).serve_forever()
