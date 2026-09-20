#!/usr/bin/env python3
"""Real process restart/cancellation and memory-only auth checks; synthetic tokens only."""
import ctypes
import json
import os
import pathlib
import queue
import subprocess
import sys
import tempfile
import threading
import urllib.request

binary = str(pathlib.Path(sys.argv[1]).resolve())
fixture_script = pathlib.Path(__file__).with_name("oauth_server.py")
browser = urllib.request.build_opener(urllib.request.ProxyHandler({}))

def reader(stream, out):
    for line in stream: out.put(line)

class Runtime:
    def __init__(self, base, root):
        self.frames = []
        self.out = queue.Queue()
        self.p = subprocess.Popen([binary, "--model", "fixture-codex", "--oauth-fixture", base], stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True, env={"PATH": os.environ["PATH"], "HOME": str(root), "CODEX_HOME": str(root / ".codex")}, cwd=root)
        threading.Thread(target=reader, args=(self.p.stdout, self.out), daemon=True).start()
        self.send(op="hello", capabilities=["sessions", "origin-command-id", "model-stream"])
        assert self.read()["model"] == "fixture-codex"
        self.send(op="open", request_id="open", session_id="s")
        assert self.read()["status"] == "ready"
    def send(self, **command):
        self.p.stdin.write(json.dumps(dict(command, version=1)) + "\n")
        self.p.stdin.flush()
    def read(self):
        line = self.out.get(timeout=10)
        self.frames.append(line)
        return json.loads(line)
    def turn(self, number, cancel=False):
        self.send(op="turn", request_id=str(number), session_id="s", turn_id=str(number), input="hello")
        sign_ins = 0
        while True:
            frame = self.read()
            if frame.get("status") == "streaming":
                event = frame["event"]
                if event["kind"] == "sign_in":
                    sign_ins += 1
                    browser.open(event["url"], timeout=5).read()
                if cancel and event["kind"] == "text":
                    self.send(op="cancel", request_id="cancel", session_id="s")
                    cancel = False
            else:
                return frame, sign_ins
    def close(self):
        self.p.stdin.close()
        assert self.p.wait(timeout=5) == 0
        log = self.p.stderr.read()
        output = "".join(self.frames) + log
        for secret in ["synthetic-refresh", "synthetic.", "synthetic-secret-must-not-leak", "installed-credential-canary"]:
            assert secret not in output, "credential escaped the Rust model service"
        return log

with tempfile.TemporaryDirectory(prefix="psi-auth-lifetime-") as scratch:
    root = pathlib.Path(scratch)
    (root / ".codex").mkdir()
    sentinel = root / ".codex/auth.json"
    sentinel.write_text('{"access_token":"installed-credential-canary"}')
    before = {str(p.relative_to(root)): p.read_bytes() for p in root.rglob("*") if p.is_file()}
    # Observe reads/opens of the installed credential directory without tracing
    # HTTP/token contents or saving tokens in a test report.
    libc = ctypes.CDLL(None, use_errno=True)
    watch = libc.inotify_init1(os.O_NONBLOCK | os.O_CLOEXEC)
    assert watch >= 0
    assert libc.inotify_add_watch(watch, os.fsencode(sentinel.parent), 0x1 | 0x20) >= 0
    try:
        for scenario in ("rotation", "refresh-failed", "hold"):
            fixture = subprocess.Popen([sys.executable, str(fixture_script), scenario], stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, text=True)
            base = fixture.stdout.readline().strip()
            try:
                runtime = Runtime(base, root)
                try:
                    reply, sign_ins = runtime.turn(1, cancel=scenario == "hold")
                    assert sign_ins == 1
                    if scenario == "rotation":
                        assert reply["status"] == "completed"
                        assert reply["provider_output"][2]["call_id"] == "call_47"
                        reply, sign_ins = runtime.turn(2)
                        assert reply["status"] == "completed" and sign_ins == 0
                    elif scenario == "refresh-failed":
                        assert reply["status"] == "failed" and reply["error"] == "refresh_failed"
                        _, sign_ins = runtime.turn(2)
                        assert sign_ins == 1, "failed rotation must discard the old refresh token"
                    else:
                        assert reply["status"] == "cancelled" and reply["error"] == "local_cancelled"
                        assert reply["cancellation_scope"] == "local_only_server_processing_and_billing_unknown"
                        assert runtime.read()["status"] == "cancel_requested"
                finally:
                    runtime.close()
                if scenario == "rotation":
                    restarted = Runtime(base, root)
                    try:
                        reply, sign_ins = restarted.turn(1)
                        assert reply["status"] == "completed" and sign_ins == 1
                    finally:
                        restarted.close()
                    evidence = json.loads(browser.open(base + "/evidence").read())
                    assert evidence["authorizations"] == 2 and evidence["rotations"] == 3
                    assert len(evidence["requests"][1]["input"]) == 5
            finally:
                fixture.terminate()
                fixture.wait(timeout=5)
        try:
            accesses = os.read(watch, 65536)
        except BlockingIOError:
            accesses = b""
        assert not accesses, "Psi accessed installed credentials"
    finally:
        os.close(watch)
    after = {str(p.relative_to(root)): p.read_bytes() for p in root.rglob("*") if p.is_file()}
    assert before == after, "runtime persisted data under its home"
print(json.dumps({"passed": True, "restart_requires_sign_in": True, "refresh_rotation": True, "refresh_failure_discards_tokens": True, "local_cancellation": True, "credential_directory_unaccessed": True, "home_unchanged": True, "protocol_and_stderr_no_tokens": True}))
