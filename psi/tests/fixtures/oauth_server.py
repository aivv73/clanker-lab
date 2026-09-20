#!/usr/bin/env python3
"""Synthetic OAuth/SSE peer. Verifies PKCE and single-use rotating refresh tokens.
No credentials or requests are written to disk or diagnostics.
"""
import base64
import hashlib
import json
import sys
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import parse_qs, urlencode, urlsplit

scenario = sys.argv[1] if len(sys.argv) > 1 else "normal"
lock = threading.Lock()
state = {"codes": {}, "refresh": None, "access": None, "authorizations": 0, "exchanges": 0, "rotations": 0, "requests": [], "disconnected": False}

def b64(data):
    return base64.urlsafe_b64encode(data).decode().rstrip("=")

def event(value):
    return ("data: " + json.dumps(value, ensure_ascii=False) + "\r\n\r\n").encode()

class Handler(BaseHTTPRequestHandler):
    def log_message(self, *_):
        pass

    def reply(self, status, value):
        body = json.dumps(value).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self):
        url = urlsplit(self.path)
        if url.path == "/evidence":
            with lock:
                return self.reply(200, {k: state[k] for k in ("authorizations", "exchanges", "rotations", "requests", "disconnected")})
        if url.path != "/authorize":
            return self.reply(404, {})
        q = parse_qs(url.query)
        assert q["code_challenge_method"] == ["S256"]
        assert q["scope"] == ["openid profile email offline_access"]
        assert len(q["code_challenge"][0]) == 43
        assert len(q["state"][0]) == 43
        with lock:
            state["authorizations"] += 1
            code = "synthetic-code-" + str(state["authorizations"])
            state["codes"][code] = (q["code_challenge"][0], q["redirect_uri"][0])
        params = {"state": q["state"][0], "code": code}
        if scenario == "state": params["state"] = "wrong"
        if scenario == "denied": params = {"state": q["state"][0], "error": "access_denied"}
        target = q["redirect_uri"][0] + "?" + urlencode(params)
        if scenario == "duplicate-state": target += "&state=" + q["state"][0]
        self.send_response(302)
        self.send_header("Location", target)
        self.end_headers()

    def do_POST(self):
        body = self.rfile.read(int(self.headers["Content-Length"]))
        if self.path == "/token":
            q = parse_qs(body.decode())
            with lock:
                if q["grant_type"] == ["authorization_code"]:
                    if scenario == "sign-in-failed": return self.reply(400, {"error": "synthetic-secret-must-not-leak"})
                    code = state["codes"].pop(q["code"][0], None)
                    assert code == (b64(hashlib.sha256(q["code_verifier"][0].encode()).digest()), q["redirect_uri"][0])
                    state["exchanges"] += 1
                else:
                    assert q["grant_type"] == ["refresh_token"]
                    assert q["refresh_token"][0] == state["refresh"]
                    state["refresh"] = None
                    if scenario == "refresh-failed": return self.reply(400, {"error": "synthetic-secret-must-not-leak"})
                    state["rotations"] += 1
                suffix = str(state["exchanges"]) + "-" + str(state["rotations"])
                state["access"] = "synthetic." + b64(json.dumps({"https://api.openai.com/auth": {"chatgpt_account_id": "fixture-account"}}).encode()) + "." + suffix
                state["refresh"] = "synthetic-refresh-" + suffix
                return self.reply(200, {"access_token": state["access"], "refresh_token": state["refresh"], "expires_in": 1 if scenario in ("rotation", "refresh-failed") else 3600})
        assert self.path == "/responses"
        assert self.headers["Authorization"] == "Bearer " + state["access"]
        assert self.headers["chatgpt-account-id"] == "fixture-account"
        request = json.loads(body)
        assert request["store"] is False and request["stream"] is True
        assert "reasoning.encrypted_content" in request["include"]
        with lock: state["requests"].append(request)
        if scenario == "401": return self.reply(401, {"error": "synthetic-secret-must-not-leak"})
        if scenario == "mime": return self.reply(200, {})
        self.send_response(200)
        if scenario == "broken": self.send_header("Content-Length", "99999")
        if not scenario.startswith("missing-mime"): self.send_header("Content-Type", "text/html" if scenario == "html-mime" else "text/event-stream; charset=utf-8")
        self.end_headers()
        text = "Psi HTTP fixture: Привет 🌍"
        output = [
            {"type": "reasoning", "id": "rs_47", "encrypted_content": "opaque-reasoning", "summary": [{"type": "summary_text", "text": "Considered the greeting."}]},
            {"type": "message", "id": "msg_47", "role": "assistant", "status": "completed", "content": [{"type": "output_text", "text": text, "annotations": []}]},
        ]
        if scenario in ("items", "rotation", "fallback", "mismatch"):
            output.append({"type": "function_call", "id": "fc_47", "call_id": "call_47", "name": "future_tool", "arguments": '{"path":"target.txt"}', "status": "completed"})
        frames = [event({"type": "response.reasoning_summary_text.delta", "delta": "Considered the greeting."}), event({"type": "response.output_text.delta", "delta": text})]
        if scenario == "invalid-utf8": frames = [b'data: {"type":"response.output_text.delta","delta":"\xff"}\n\n']
        elif scenario == "invalid-json": frames = [b'data: nope\n\n']
        elif scenario == "limit": frames = [b':' + b'x' * (512 * 1024)]
        elif scenario == "incomplete":
            frames.append(event({"type": "response.output_item.done", "output_index": 0, "item": output[0]}))
            frames.append(event({"type": "response.incomplete", "response": {"status": "incomplete"}}))
        elif scenario == "failed": frames.append(event({"type": "response.failed", "response": {"error": "synthetic-secret-must-not-leak"}}))
        elif scenario not in ("unfinished", "broken", "hold", "done-only", "missing-mime-unfinished", "missing-mime-json", "missing-mime-html"):
            frames.extend(event({"type": "response.output_item.done", "output_index": i, "item": item}) for i, item in enumerate(output))
            frames.append(event({"type": "response.completed", "response": {"id": "resp_47", "status": "completed", "output": [] if scenario == "fallback" else output[1:] if scenario == "mismatch" else output}}))
        if scenario == "done-only": frames.append(b'data: [DONE]\n\n')
        wire = b'\xef\xbb\xbf: fixture\r\n\r\n' + b''.join(frames)
        if scenario == "missing-mime-json": wire = b'{"message":"not an SSE completion"}'
        if scenario == "missing-mime-html": wire = b'<html>not an SSE completion</html>\n\n'
        if scenario == "cr": wire = wire.replace(b'\r\n', b'\r')
        if scenario == "multiline": wire = wire.replace(b'data: {"type":', b'data: {\ndata: "type":')
        try:
            # Byte-sized writes split Unicode and CRLF. TCP may coalesce: Rust unit
            # coverage additionally feeds every possible split deterministically.
            for byte in wire:
                self.wfile.write(bytes([byte]))
            self.wfile.flush()
            if scenario == "hold":
                for _ in range(600):
                    time.sleep(.05)
                    self.wfile.write(b': waiting\n\n')
                    self.wfile.flush()
        except (BrokenPipeError, ConnectionResetError):
            state["disconnected"] = True

server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
server.daemon_threads = True
print(f"http://127.0.0.1:{server.server_port}", flush=True)
server.serve_forever()
