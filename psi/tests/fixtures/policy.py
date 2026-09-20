"""Real controller lifecycle around the real CPU policy child; no tool execution."""
import atexit
import json
import pathlib
import queue
import re
import sqlite3
import subprocess
import sys
import tempfile
import threading
import time

binary = sys.argv[1]

class Runtime:
    def __init__(self, state, *options):
        self.state = state
        self.p = subprocess.Popen([binary,"--state-dir",str(state),"--agent-loop","policy-fixture",*options],stdin=subprocess.PIPE,stdout=subprocess.PIPE,stderr=subprocess.PIPE,text=True)
        def reap():
            if self.p.poll() is None: self.p.kill(); self.p.wait()
        atexit.register(reap)
        self.frames = queue.Queue()
        self.logs = queue.Queue()
        self.log = []
        def collect(stream, target, parse):
            for line in stream:
                target.put(parse(line))
        self.threads = [threading.Thread(daemon=True,target=collect,args=(self.p.stdout,self.frames,json.loads)),threading.Thread(daemon=True,target=collect,args=(self.p.stderr,self.logs,lambda line:(time.monotonic(),line.strip())))]
        for thread in self.threads: thread.start()
        self.send(dict(op="hello",capabilities=["model-stream","durable-events"]))
        self.send(dict(op="open",request_id="open",session_id="s"))
    def write(self, command):
        self.p.stdin.write(json.dumps(dict(version=1,**command))+"\n");self.p.stdin.flush()
    def read(self): return self.frames.get(timeout=5)
    def send(self, command):
        self.write(command)
        while True:
            result = self.read()
            if result.get("status") not in ("accepted","streaming"): return result
    def child(self):
        while True:
            item = self.logs.get(timeout=5);self.log.append(item)
            match = re.search(r"Psi policy child: (\d+)",item[1])
            if match: return int(match[1])
    def close(self):
        if not self.p.stdin.closed: self.p.stdin.close()
        try: assert self.p.wait(timeout=5)==0
        finally:
            if self.p.poll() is None: self.p.kill();self.p.wait()
            for thread in self.threads: thread.join(timeout=2)
            while not self.logs.empty(): self.log.append(self.logs.get())
        children = [int(match[1]) for _,line in self.log if (match:=re.search(r"Psi policy child: (\d+)",line))]
        for pid in children:
            assert not pathlib.Path(f"/proc/{pid}").exists(),f"surviving policy child {pid}"
            assert any(line==f"Psi policy reaped: {pid}" for _,line in self.log)
        return children

turn = dict(op="turn",request_id="origin",session_id="s",turn_id="t",input="fixture action")
with tempfile.TemporaryDirectory(prefix="psi-policy-controller-") as tmp:
    root = pathlib.Path(tmp)
    runtime = Runtime(root/"normal")
    first = runtime.send(turn)
    assert first["status"]=="completed" and first["policy"]["handoffs"]==1
    assert runtime.send(turn)["duplicate"]
    assert runtime.send(dict(turn,request_id="second",turn_id="t2"))["policy"]["runtime_counter"]==2
    runtime.close()
    restarted = Runtime(root/"normal")
    assert restarted.send(turn)["policy"]==first["policy"]
    fresh = restarted.send(dict(turn,request_id="third",turn_id="t3"))
    assert fresh["policy"]["epoch"] != first["policy"]["epoch"]
    assert fresh["policy"]["runtime_counter"]==1, "replay must not execute the action"
    restarted.close()
    # Controlling EOF during a hung policy call, separately from ordinary Cancel.
    for control in ("cancel","eof"):
        runtime = Runtime(root/control,"--policy-fault","hang")
        runtime.write(turn)
        assert runtime.read()["status"]=="accepted"
        pid = runtime.child()
        started = time.monotonic()
        if control=="cancel":
            runtime.write(dict(op="cancel",request_id="cancel",session_id="s",turn_id="t"))
            result = runtime.read()
            assert result["status"]=="cancelled" and result["policy"]["handoffs"]==0
            assert runtime.read()["status"]=="cancel_requested"
        runtime.close()
        reaped = next(stamp for stamp,line in runtime.log if line==f"Psi policy reaped: {pid}")
        assert reaped-started < .2, "active policy child must be reaped without waiting for its decision timeout"
        db = sqlite3.connect(runtime.state/"history.sqlite")
        result = json.loads(db.execute("SELECT result FROM commands WHERE id='origin'").fetchone()[0]);db.close()
        assert result["status"]=="cancelled" and result["policy"]["handoffs"]==0
        assert any(t["result"].get("Err")=="policy_cancelled" for t in result["policy"]["transitions"])
    # A cancelled reply cannot be revived by releasing the fixture afterward.
    release = root/"release"
    runtime = Runtime(root/"held","--policy-gate",str(release))
    runtime.write(turn)
    assert runtime.read()["status"]=="accepted"
    assert "Permit received" in runtime.read()["event"]["delta"]
    runtime.write(dict(op="cancel",request_id="cancel",session_id="s",turn_id="t"))
    result = runtime.read()
    assert result["status"]=="cancelled" and result["policy"]["handoffs"]==0
    assert result["policy"]["state"]=="Cancelled"
    assert runtime.read()["status"]=="cancel_requested"
    release.write_text("late")
    again = runtime.send(turn)
    assert again["duplicate"] and again["policy"]==result["policy"]
    next_turn = runtime.send(dict(turn,request_id="next",turn_id="next-turn"))
    assert next_turn["status"]=="completed" and next_turn["policy"]["runtime_counter"]==1
    runtime.close()
print("Real policy controller: handoff, deduplication, restart epoch, pending cancellation, EOF and late release passed")
