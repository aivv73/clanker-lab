#!/usr/bin/env python3
"""Run real T3 web/server with a fresh fixture-only home and Git workspace."""
import argparse
import json
import os
import pathlib
import signal
import subprocess
import tempfile
from prepare import HERE, prepare

parser = argparse.ArgumentParser()
parser.add_argument("checkout")
parser.add_argument("--agent-loop", choices=["single-response", "buffered-response"], default="single-response")
parser.add_argument("--startup-fault", choices=["missing", "cycle", "duplicate", "partial"])
parser.add_argument("--hold-fixture", action="store_true")
parser.add_argument("--port", type=int, default=8844)
parser.add_argument("--web-port", type=int, default=8845)
args = parser.parse_args()
t3 = prepare(args.checkout)
subprocess.run(["cargo", "build", "--locked"], cwd=HERE.parent, check=True)
root = pathlib.Path(tempfile.mkdtemp(prefix="psi-t3-"))
(root / "userdata").mkdir()
workspace = root / "workspace"
workspace.mkdir()
(workspace / "README.md").write_text("Psi fixture workspace. No tools are enabled.\n")
subprocess.run(["git", "init", "-q", str(workspace)], check=True)
subprocess.run(["git", "add", "."], cwd=workspace, check=True)
subprocess.run(["git", "-c", "user.name=Psi", "-c", "user.email=psi@localhost", "commit", "-qm", "fixture"], cwd=workspace, check=True)
instances = {name: {"driver": name, "enabled": False, "config": {}} for name in ["codex", "claudeAgent", "cursor", "grok", "opencode", "antigravity"]}
instances["psi"] = {"driver": "psi", "displayName": "Psi", "enabled": True, "environment": [], "config": {"binary": str(HERE.parent / "target/debug/psi")}}
instances["psi"]["config"]["agentLoop"] = args.agent_loop
if args.startup_fault:
    instances["psi"]["config"]["startupFault"] = args.startup_fault
if args.hold_fixture:
    instances["psi"]["config"]["fixtureGate"] = str(root / "release-fixture")
(root / "userdata/settings.json").write_text(json.dumps({"providerInstances": instances}))
# Avoid inheriting an ambient live T3 home or baked browser origin.
env = {k: v for k, v in os.environ.items() if k not in {"T3CODE_HOME", "VITE_HTTP_URL", "VITE_WS_URL", "T3CODE_DEV_AUTH_TOKEN"}}
web_env = dict(env, PORT=str(args.web_port), T3CODE_PORT=str(args.port), T3CODE_SINGLE_ORIGIN_DEV="1", HOST="127.0.0.1")
processes = []
for sig in (signal.SIGINT, signal.SIGTERM):
    signal.signal(sig, lambda *_: (_ for _ in ()).throw(KeyboardInterrupt()))
try:
    processes.append(subprocess.Popen(["node", "apps/server/src/bin.ts", "--base-dir", str(root), "--port", str(args.port), "--host", "127.0.0.1", "--no-browser", "--auto-bootstrap-project-from-cwd", str(workspace)], cwd=t3, env=env, start_new_session=True))
    processes.append(subprocess.Popen([str(t3 / "node_modules/.bin/vp"), "dev"], cwd=t3 / "apps/web", env=web_env, start_new_session=True))
    (root / "run.json").write_text(json.dumps({"root": str(root), "server_pid": processes[0].pid, "web_pid": processes[1].pid, "port": args.port, "web_port": args.web_port}))
    print(f"Psi scratch root: {root}\nWeb: http://127.0.0.1:{args.web_port}\nUse the server pairing URL with the web port.", flush=True)
    processes[0].wait()
except KeyboardInterrupt:
    pass
finally:
    for process in processes:
        if process.poll() is None:
            process.terminate()
    for process in processes:
        try:
            process.wait(timeout=10)
        except subprocess.TimeoutExpired:
            os.killpg(process.pid, signal.SIGKILL)
            process.wait()
        finally:
            # The Vite launcher can exit before its child server. These groups
            # were created by this launcher; never signal unrelated processes.
            try:
                os.killpg(process.pid, signal.SIGTERM)
            except ProcessLookupError:
                pass
