"""A06/A09/A10: real Bend permits, isolated process effects and durable pairs."""
import json, pathlib, shlex, sqlite3, subprocess, sys, tempfile, time
root = pathlib.Path(tempfile.mkdtemp(prefix='psi-continuation-'))
workspace = root/'workspace'; workspace.mkdir()
log = open(root/'runtime.log', 'w')
p = subprocess.Popen([str(pathlib.Path(sys.argv[1]).resolve()), '--state-dir', str(root/'state'), '--agent-loop', 'exec-fixture'], stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=log, text=True)
def send(**frame):
    p.stdin.write(json.dumps(dict(version=1, **frame))+'\n'); p.stdin.flush()
def read():
    line = p.stdout.readline(); assert line, (p.poll(), root)
    return json.loads(line)
def tool(name, **arguments): return dict(name=name, arguments=arguments)
def poll(**arguments): return tool('write_stdin', session_id='$process', yield_time_ms=20, **arguments)
def run(commands, turn):
    send(op='turn', request_id=turn, session_id='s', turn_id=turn, input=json.dumps(dict(commands=commands)))
    frames=[]
    while True:
        frame=read(); frames.append(frame)
        if frame.get('status') not in ('accepted','streaming'): break
    assert frame['status']=='completed', (frame,root)
    items=frame['provider_output'][:-1]
    values=[]
    transitions=[t for t in frame['policy']['transitions'] if t['event']=='Request']
    assert len(transitions)==len(items)//2
    assert len({t['request_id'] for t in transitions})==len(transitions)
    for call,transition in zip(items[::2],transitions):
        assert transition['action']==dict(id=call['call_id'],name=call['name'],arguments=json.loads(call['arguments']))
        assert transition['turn_id']==turn and transition['epoch']==frame['policy']['epoch']
        assert transition['result']=={'Ok':['Active', True]},transition
    db=sqlite3.connect(root/'state/history.sqlite')
    for call,result in zip(items[::2],items[1::2]):
        assert call['call_id']==result['call_id']
        value=json.loads(result['output']); values.append(value)
        if value['reason']!='rejected':
            row=db.execute('SELECT intent,state,result FROM actions WHERE id=?',(call['call_id'],)).fetchone()
            assert row and row[1]=='result' and json.loads(row[2])==value,(row,value)
            assert json.loads(row[0])==dict(name=call['name'],arguments=json.loads(call['arguments']))
    lifetimes=db.execute('SELECT result FROM command_processes WHERE turn=?',(turn,)).fetchall()
    assert lifetimes and all(json.loads(row[0])['cleanup_confirmed'] for row in lifetimes),lifetimes
    history=json.loads(db.execute('SELECT history FROM sessions WHERE id="s"').fetchone()[0])
    for item in items: assert item in history,(item,history)
    db.close()
    assert any(f.get('status')=='streaming' for f in frames)
    return values
try:
    send(op='hello', capabilities=['sessions','origin-command-id','model-stream','durable-events']); read()
    send(op='open', request_id='open', session_id='s', workspace=str(workspace)); assert read()['status']=='ready'
    code="import os,pathlib,sys; pathlib.Path('started').write_text(str(os.getpid())); print('ready',flush=True); line=input(); pathlib.Path('continued').write_text(str(os.getpid())+':'+line); print('received:'+line,flush=True)"
    values=run([
        tool('exec_command',cmd='python3 -c '+shlex.quote(code),yield_time_ms=50),
        tool('exec_command',cmd='touch competing'),
        tool('apply_patch',patch='*** Begin Patch\n*** Add File: competing\n+bad\n*** End Patch'),
        tool('write_stdin',session_id='foreign-process',chars='bad\n'),
        poll(), tool('write_stdin',session_id='$process',chars='hello\n',yield_time_ms=1000),
        tool('exec_command',cmd='touch after-cleanup',yield_time_ms=30000),
    ],'interactive')
    assert values[0]['reason']=='running',values
    assert ''.join(v.get('output','') for v in values)=='ready\nreceived:hello\n',values
    assert [v.get('error') for v in values[1:4]]==['tool_busy','tool_busy','unknown_process'],values
    assert values[4]['reason']=='running',values
    assert values[5]['reason']=='exit' and 'received:hello' in values[5]['output'],values
    assert (workspace/'continued').read_text()==(workspace/'started').read_text()+':hello'
    assert not (workspace/'competing').exists() and (workspace/'after-cleanup').exists()
    stale=values[0]['session_id']
    values=run([tool('exec_command',cmd='sleep 0.3',yield_time_ms=20), tool('write_stdin',session_id=stale), poll()], 'stale')
    assert values[1]['error']=='unknown_process',values
    start=time.monotonic()
    values=run([tool('exec_command',cmd='sleep 10',yield_time_ms=20,timeout_ms=250)]+[poll() for _ in range(3)],'deadline')
    assert values[-1]['reason']=='timeout' and time.monotonic()-start<3,values
    totals=[v['output_bytes'] for v in values if 'output_bytes' in v]
    assert totals==sorted(totals)
    code="import os,sys\nfor line in sys.stdin:\n os.write(1,b'x'*(3*1024*1024)); print('chunk',flush=True)"
    values=run([tool('exec_command',cmd='python3 -c '+shlex.quote(code),yield_time_ms=20,max_output_tokens=100)]+[poll(chars='next\n',max_output_tokens=100) for _ in range(4)], 'output')
    assert values[-1]['reason']=='output_limit',values
    assert values[-1]['output_bytes']>10*1024*1024,values[-1]
    assert all(len(v.get('output',''))<=100 for v in values[:5]),values
    assert all(len(v.get('output',''))<=16000 for v in values),values
    assert any(v.get('truncated') for v in values)
    # Cancel after the process identity has been returned, while polling is live.
    send(op='turn',request_id='cancelled',session_id='s',turn_id='cancelled',input=json.dumps(dict(cmd='sleep 20',yield_time_ms=0)))
    while True:
        frame=read()
        item=frame.get('event',{}).get('item',{})
        if item.get('type')=='function_call_output' and json.loads(item['output'])['reason']=='running': break
        assert frame.get('status') in ('accepted','streaming'),frame
    start=time.monotonic()
    send(op='cancel',request_id='stop',session_id='s',turn_id='cancelled')
    while True:
        frame=read()
        if frame.get('status')=='cancelled': break
    assert time.monotonic()-start<4.5 and frame['tool_result']['cleanup_confirmed'],frame
    assert frame['policy']['closed'] and frame['policy']['state']=='Cancelled',frame
    assert read()['status']=='cancel_requested'
    db=sqlite3.connect(root/'state/history.sqlite')
    value=json.loads(db.execute("SELECT result FROM command_processes WHERE turn='cancelled'").fetchone()[0])
    assert value['cleanup_confirmed'] and value['reason']=='cancelled',value
    db.close()
    # No pending read is needed to enforce the command's original deadline.
    probe=subprocess.Popen(['systemd-run','--user','--quiet','--pipe','--wait','--collect','--service-type=exec','-p','Delegate=memory pids',str(pathlib.Path(sys.argv[1]).resolve()),'--exec-supervisor',str(workspace)],stdin=subprocess.PIPE,stdout=subprocess.PIPE,stderr=log,text=True)
    probe.stdin.write(json.dumps(dict(cmd='sleep 10',yield_time_ms=0,timeout_ms=200))+'\n');probe.stdin.flush()
    assert json.loads(probe.stdout.readline())['reason']=='running'
    final=json.loads(probe.stdout.readline())
    assert final['reason']=='timeout' and final['cleanup_confirmed'],final
    probe.stdin.close();probe.wait(timeout=6)
    print(json.dumps(dict(passed=True,root=str(root),checks=['continued_stdin','read_only_poll','busy_exec_patch','foreign_and_stale_identity','deadline','aggregate_output','durable_invocation_pairs','fresh_bound_permits','cancel_after_yield','idle_deadline'])))
finally:
    p.stdin.close(); p.wait(timeout=10); log.close()
