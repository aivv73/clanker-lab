import json
import pathlib
import sqlite3
import subprocess
import sys
import tempfile

binary = sys.argv[1]
with tempfile.TemporaryDirectory(prefix="psi-durable-") as temp:
    root = pathlib.Path(temp)
    state = root / "state"
    def start(gate=False):
        p = subprocess.Popen([binary,"--state-dir",str(state),*(["--fixture-gate",str(root/"never")] if gate else [])],stdin=subprocess.PIPE,stdout=subprocess.PIPE,stderr=subprocess.PIPE,text=True)
        send(p,dict(op="hello",capabilities=["model-stream","durable-events"]))
        send(p,dict(op="open",request_id="open",session_id="s",workspace=temp + "/workspace"))
        return p
    def write(p,c):
        p.stdin.write(json.dumps(dict(version=1,**c))+"\n");p.stdin.flush()
    def read(p):
        line=p.stdout.readline()
        assert line, p.stderr.read()
        return json.loads(line)
    def send(p,c):
        write(p,c)
        frames=[]
        while True:
            f=read(p);frames.append(f)
            if f.get("status") not in ("streaming","accepted") and not f.get("replay"):
                return f,frames
    (root/"workspace").mkdir()
    c=dict(op="turn",request_id="origin",session_id="s",turn_id="turn",input="hello")
    p=start()
    result,frames=send(p,c)
    assert result["output"] == "Psi fixture: hello"
    acceptance=frames[0]
    assert acceptance["status"] == "accepted" and acceptance["seq"] == 1
    p.stdin.close();assert p.wait(timeout=5)==0
    db=sqlite3.connect(state/"history.sqlite")
    history=db.execute("SELECT history FROM sessions").fetchone()[0]
    p=start()
    again,replies=send(p,c)
    assert again["duplicate"] and again["output"]==result["output"]
    assert replies[0]["seq"]==acceptance["seq"]
    assert send(p,dict(c,input="conflict"))[0]["error"]=="request_id_conflict"
    assert send(p,dict(op="history",request_id="history",session_id="s"))[0]["history"] == json.loads(history)
    _, replay=send(p,dict(op="replay",request_id="replay",session_id="s",after=0))
    assert [f["seq"] for f in replay[:-1]]==list(range(1,len(frames)+1))
    p.stdin.close();assert p.wait(timeout=5)==0
    assert b"turn admitted" not in p.stderr.read().encode()
    # Crash after acceptance committed, before the fixture can produce a result.
    p=start(True)
    c2=dict(c,request_id="crash",turn_id="crash-turn")
    write(p,c2)
    accepted=read(p)
    assert accepted["status"]=="accepted"
    write(p,c2)
    retried=read(p)
    assert retried["status"]=="accepted" and retried["duplicate"] and retried["seq"]==accepted["seq"]
    assert send(p,dict(c2,input="conflicting pending request"))[0]["error"]=="request_id_conflict"
    assert db.execute("SELECT result FROM commands WHERE id='crash'").fetchone()[0] is None
    p.kill();p.wait()
    p=start()
    result,_=send(p,c2)
    assert result["error"]=="interrupted_outcome_unknown" and result["duplicate"]
    assert send(p,dict(c,request_id="new",turn_id="new-turn"))[0]["error"]=="session_outcome_unknown"
    p.stdin.close();assert p.wait(timeout=5)==0
    assert "turn admitted" not in p.stderr.read()
    assert db.execute("SELECT history FROM sessions").fetchone()[0]==history
    db.close()
