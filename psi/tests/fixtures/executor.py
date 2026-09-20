"""Real Rust/Bend/systemd/bwrap checks. Missing host prerequisites FAIL, never skip."""
import json, os, pathlib, select, shlex, socket, sqlite3, subprocess, sys, tempfile, time
P = pathlib.Path
binary = str(P(sys.argv[1]).resolve())
root = P(tempfile.mkdtemp(prefix='psi-executor-'))
ws = root/'workspace'; ws.mkdir()
control = root/'control'; control.write_text('unchanged')
(ws/'escape').symlink_to(control)
unix = socket.socket(socket.AF_UNIX); unix.bind(str(ws/'host.sock')); unix.listen(); unix.setblocking(False)
tcp = socket.socket(); tcp.bind(('127.0.0.1',0)); tcp.listen(); tcp.setblocking(False)
log = open(root/'runtime.log','w')
p = subprocess.Popen([binary,'--state-dir',str(root/'state'),'--agent-loop','exec-fixture'],stdin=subprocess.PIPE,stdout=subprocess.PIPE,stderr=log,text=True)
counter = 0

def send(frame):
    p.stdin.write(json.dumps(dict(version=1,**frame))+'\n');p.stdin.flush()
def read():
    # Buffered readline can hold another frame; rely on the outer test timeout.
    line=p.stdout.readline(); assert line, f'Rust exited: {p.poll()}, {root}'
    return json.loads(line)
def terminal():
    frames=[]
    while True:
        f=read();frames.append(f)
        if f.get('status') not in ('accepted','streaming'): return f

def execute(args):
    global counter
    counter+=1
    send(dict(op='turn',request_id=f'r{counter}',session_id='s',turn_id=f't{counter}',input=json.dumps(args)))
    frame=terminal()
    assert frame.get('status')=='completed', (frame,root)
    call,result=frame['provider_output'][:2]
    assert call['name']=='exec_command' and call['call_id']==result['call_id']
    value=json.loads(result['output']);assert value['cleanup_confirmed'],value
    db=sqlite3.connect(root/'state/history.sqlite')
    row=db.execute('SELECT state,result FROM actions WHERE turn=?',(f't{counter}',)).fetchone();db.close()
    assert row[0]=='result' and json.loads(row[1])==value
    if 'cgroup' in value: assert not P(value['cgroup']).exists(),value
    return value

def python(code,**args): return execute(dict(cmd='python3 -c '+shlex.quote(code),**args))
try:
    send(dict(op='hello',capabilities=['sessions','origin-command-id','model-stream','durable-events']));read()
    send(dict(op='open',request_id='open',session_id='s',workspace=str(ws)));assert read()['status']=='ready'
    code=f'''
import os,pathlib,socket,stat,ctypes,errno,json
p=pathlib.Path
p('target.txt').write_text('value=new\\n')
checks={{}}
for name,path in [('symlink','escape'),('home',{str(P.home()/'.ssh')!r}),('cgroup','/sys/fs/cgroup'),('host_run','/run/user')]:
 try: p(path).read_bytes(); checks[name]=False
 except OSError: checks[name]=True
try: p('/usr/psi-test').write_text('bad');checks['readonly']=False
except OSError as e: checks['readonly']=e.errno in (errno.EROFS,errno.EACCES)
for name,op in [('unix',lambda:socket.socket(socket.AF_UNIX)),('socketpair',socket.socketpair),('tcp',lambda:socket.create_connection(('127.0.0.1',{tcp.getsockname()[1]}),.1))]:
 try: op();checks[name]=False
 except OSError: checks[name]=True
lib=ctypes.CDLL(None,use_errno=True)
checks['io_uring']=lib.syscall(425,1,0)==-1 and ctypes.get_errno()==errno.EPERM
checks['fds']=all(not stat.S_ISSOCK(os.fstat(fd).st_mode) for fd in (0,1,2))
checks['environment']=dict(os.environ).keys() <= {{'PATH','HOME','LANG','TMPDIR','PWD','LC_CTYPE','SHLVL','_'}}
checks['tmp']=os.statvfs('/tmp').f_blocks*os.statvfs('/tmp').f_frsize==268435456
assert all(checks.values()),checks
print(json.dumps(checks))
'''
    result=python(code); assert result['reason']=='exit' and result['exit_code']==0,result
    assert (ws/'target.txt').read_text()=='value=new\n' and control.read_text()=='unchanged'
    assert not select.select([unix,tcp],[],[],0)[0], 'host listener was contacted'
    assert execute(dict(cmd='exit 7'))['exit_code']==7
    # bwrap rejects a nonexistent cwd before a payload can write a marker.
    result=execute(dict(cmd='touch /workspace/must-not-run',workdir='/workspace/absent'))
    assert result['reason']=='launch_failure' and result['exit_code']!=0 and not (ws/'must-not-run').exists(),result
    # Conservative per-response truncation and aggregate output termination.
    result=python("import os; os.write(1,b'x'*20000)",max_output_tokens=100)
    assert result['truncated'] and len(result['output'])<=100,result
    result=python("import os\nwhile True: os.write(1,b'x'*65536)")
    assert result['reason']=='output_limit' and result['truncated'],result
    result=python("import time; time.sleep(10)",timeout_ms=100)
    assert result['reason']=='timeout',result
    # Normal exit must clean an independently-sessioned surviving child.
    result=python("import os,time\nif os.fork()==0:\n os.setsid(); time.sleep(30)\nelse: print('parent exited',flush=True)")
    assert result['reason']=='exit' and result['exit_code']==0,result
    # A temp ENOSPC is an executed command result; no invented workspace quota.
    result=python("import errno\ntry:\n f=open('/tmp/full','wb',buffering=0)\n for _ in range(257): f.write(b'x'*1048576)\nexcept OSError as e:\n assert e.errno==errno.ENOSPC; print('ENOSPC')")
    assert result['exit_code']==0 and 'ENOSPC' in result['output'],result
    for bad in [dict(cmd='touch must-not-run',tty=True),dict(cmd='touch must-not-run',login=True),dict(cmd='touch must-not-run',sandbox_permissions='require_escalated'),dict(cmd='touch must-not-run',timeout_ms=600001),dict(cmd='touch must-not-run',workdir='/etc')]:
        counter+=1
        send(dict(op='turn',request_id=f'r{counter}',session_id='s',turn_id=f't{counter}',input=json.dumps(bad)))
        rejected=terminal(); assert rejected['status']=='failed' and rejected['error'] in ['unsupported_exec_arguments','unsupported_workdir'],rejected
        assert not (ws/'must-not-run').exists()
    result=python("import threading,time\nthreads=[]\ntry:\n for _ in range(150):\n  t=threading.Thread(target=lambda:time.sleep(.2));t.start();threads.append(t)\nexcept RuntimeError: print('pids limit')\nfor t in threads: t.join()")
    assert result['reason']=='resource_limit' and 'pids limit' in result['output'],result
    # Close the controlling channel while fork/setsid descendants ignore TERM.
    counter+=1
    code="import os,signal,time,pathlib\nsignal.signal(signal.SIGTERM,signal.SIG_IGN)\nif os.fork()==0: os.setsid()\npathlib.Path('ready').touch()\nwhile True: time.sleep(1)"
    send(dict(op='turn',request_id=f'r{counter}',session_id='s',turn_id=f't{counter}',input=json.dumps(dict(cmd='python3 -c '+shlex.quote(code)))))
    until=time.monotonic()+10
    while not (ws/'ready').exists() and time.monotonic()<until:time.sleep(.01)
    assert (ws/'ready').exists(),root
    start=time.monotonic();p.stdin.close();p.wait(timeout=6);assert time.monotonic()-start<4.5
    db=sqlite3.connect(root/'state/history.sqlite')
    result=json.loads(db.execute('SELECT result FROM actions WHERE turn=?',(f't{counter}',)).fetchone()[0])
    assert result['cleanup_confirmed'] and result['reason']=='cancelled',result
    assert not P(result['cgroup']).exists()
    assert control.read_text()=='unchanged'
    # Remove real OS prerequisites in disposable service namespaces. No test-only
    # sandbox substitute or unrestricted execution path is used.
    for prop in ['InaccessiblePaths=/usr/bin/bwrap','RestrictNamespaces=yes','InaccessiblePaths=/sys/fs/cgroup','SystemCallFilter=~seccomp prctl','Delegate=no']:
        probe=subprocess.Popen(['systemd-run','--user','--quiet','--pipe','--wait','--collect','--service-type=exec','-p','Delegate=memory pids','-p',prop,binary,'--exec-supervisor',str(ws)],stdin=subprocess.PIPE,stdout=subprocess.PIPE,stderr=subprocess.PIPE,text=True)
        probe.stdin.write(json.dumps({'cmd':'touch must-not-run'})+'\n');probe.stdin.flush()
        value=json.loads(probe.stdout.readline());probe.stdin.close();probe.wait(timeout=10)
        assert value['reason']=='launch_failure' and value['cleanup_confirmed'],(prop,value)
        assert not (ws/'must-not-run').exists(),prop
    # The real controller cannot reach a deliberately absent user manager.
    env=dict(os.environ,DBUS_SESSION_BUS_ADDRESS='unix:path=/nonexistent/psi-bus',XDG_RUNTIME_DIR='/nonexistent/psi-runtime')
    missing=subprocess.Popen([binary,'--state-dir',str(root/'missing-manager'),'--agent-loop','exec-fixture'],stdin=subprocess.PIPE,stdout=subprocess.PIPE,stderr=log,text=True,env=env)
    for command in [dict(op='hello',capabilities=['sessions','origin-command-id','model-stream','durable-events']),dict(op='open',request_id='open',session_id='s',workspace=str(ws)),dict(op='turn',request_id='turn',session_id='s',turn_id='t',input=json.dumps({'cmd':'touch must-not-run'}))]:
        missing.stdin.write(json.dumps(dict(version=1,**command))+'\n');missing.stdin.flush()
        while True:
            frame=json.loads(missing.stdout.readline())
            if frame.get('status') not in ['streaming','accepted']:break
    assert frame['status']=='completed' and frame['tool_result']['reason']=='launch_failure' and frame['tool_result']['error']=='preflight_user_systemd',frame
    assert not (ws/'must-not-run').exists()
    missing.stdin.close();missing.wait(timeout=5)
    print(json.dumps(dict(passed=True,root=str(root),checks=['boundaries','listeners','limits','durable_result','normal_descendants','controller_eof'])))
finally:
    if p.poll() is None:
        p.stdin.close();p.wait(timeout=10)
    unix.close();tcp.close();log.close()
