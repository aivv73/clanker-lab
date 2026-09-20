"""A07/A08: real descendants, durable cleanup evidence and global refusal."""
import json, os, pathlib, shlex, signal, sqlite3, subprocess, sys, tempfile, time
P = pathlib.Path
binary = str(P(sys.argv[1]).resolve())
root = P(tempfile.mkdtemp(prefix='psi-cancellation-'))
reports = []
for fault in ('', 'soft', 'observation', 'kill', 'policy-hang', 'policy-missing'):
    restart_probe = None
    failed = fault in ('observation', 'kill')
    case = root/(fault or 'normal'); case.mkdir()
    ws = case/'workspace'; ws.mkdir()
    log = open(case/'runtime.log', 'w')
    p = subprocess.Popen([binary, '--state-dir', str(case/'state'), '--agent-loop', 'exec-fixture'],
        stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=log, text=True,
        env=dict(os.environ, PSI_TEST_CLEANUP_FAULT=fault, PSI_TEST_POLICY_FAULT_FILE=str(case/'policy-fault')))
    def send(**frame):
        p.stdin.write(json.dumps(dict(version=1, **frame))+'\n'); p.stdin.flush()
    def read():
        line = p.stdout.readline(); assert line, (p.poll(), case)
        return json.loads(line)
    def terminal():
        while True:
            frame = read()
            if frame.get('status') not in ('accepted', 'streaming'): return frame
    try:
        send(op='hello', capabilities=['sessions','origin-command-id','model-stream','durable-events']); read()
        for session in ('s', 'other'):
            send(op='open', request_id='open-'+session, session_id=session, workspace=str(ws)); assert read()['status']=='ready'
        code = "import os,signal,time,pathlib\nsignal.signal(signal.SIGTERM,signal.SIG_IGN)\nif os.fork()==0:\n os.setsid(); pathlib.Path('child-ready').write_text('child')\nelse:\n pathlib.Path('parent-ready').write_text('applied change remains')\nwhile True: time.sleep(.1)"
        send(op='turn', request_id='hung', session_id='s', turn_id='hung', input=json.dumps(dict(cmd='python3 -c '+shlex.quote(code),yield_time_ms=200)))
        until=time.monotonic()+10
        while (not (ws/'child-ready').exists() or not (ws/'parent-ready').exists()) and time.monotonic()<until:
            time.sleep(.01)
        assert (ws/'child-ready').exists() and (ws/'parent-ready').exists(),case
        cg = P(P(f'/tmp/psi-external-command-{os.getuid()}.lock').read_text())
        assert 'populated 1' in (cg/'cgroup.events').read_text()
        members = [int(pid) for pid in (cg/'cgroup.procs').read_text().split()]
        assert len(members)>=3, members
        if fault.startswith('policy-'):
            (case/'policy-fault').write_text(fault.removeprefix('policy-'))
            if fault == 'policy-hang':
                # Observe the actual stopped Bend child before cancellation.
                until=time.monotonic()+2
                stopped=None
                while time.monotonic()<until and stopped is None:
                    for child in P(f'/proc/{p.pid}/task/{p.pid}/children').read_text().split():
                        try:
                            if '\nState:\tT' in P(f'/proc/{child}/status').read_text(): stopped=int(child)
                        except FileNotFoundError: pass
                    time.sleep(.002)
                assert stopped is not None,case
            else:
                time.sleep(.3)  # The next continuation fails without a usable Bend transition.
        start=time.monotonic()
        send(op='cancel' , request_id='stop', session_id='s', turn_id='hung')
        result=terminal(); elapsed=time.monotonic()-start
        assert elapsed<4.75,(elapsed,result)
        assert read()['status']=='cancel_requested'
        assert result['policy']['closed'],result
        if not fault.startswith('policy-'): assert result['policy']['state']=='Cancelled',result
        assert result['tool_result']['cleanup_confirmed'] == (not failed),result
        assert (ws/'parent-ready').read_text()=='applied change remains'
        db=sqlite3.connect(case/'state/history.sqlite')
        saved=json.loads(db.execute("SELECT result FROM commands WHERE id='hung'").fetchone()[0])
        assert saved==result,(saved,result)
        process=json.loads(db.execute("SELECT result FROM command_processes WHERE turn='hung'").fetchone()[0])
        assert process==result['tool_result']
        if failed:
            assert result['status']=='failed' and result['remaining_resources'],result
            assert process['reason']=='cleanup_failure'
            for session in ('s','other'):
                send(op='turn',request_id='later-'+session,session_id=session,turn_id='later-'+session,input=json.dumps(dict(cmd='touch forbidden')))
                later=terminal(); assert later['error']=='runtime_unhealthy',later
            assert not (ws/'forbidden').exists()
        else:
            assert result['status']==('failed' if fault == 'policy-missing' else 'cancelled'),result
            if fault.startswith('policy-'):
                assert any(t['result'].get('Err','').startswith('policy_') for t in result['policy']['transitions']),result
                (case/'policy-fault').unlink()
            evidence=process['cleanup']
            assert evidence['populated'] is False and evidence['owned_processes_reaped'] and evidence['output_pipes_drained'],evidence
            if fault == 'soft':
                assert evidence['forced_termination'] and 1900 <= evidence['soft_termination_ms'] < 2200,evidence
            assert not cg.exists()
            assert all(not P(f'/proc/{pid}').exists() for pid in members),members
            send(op='turn',request_id='later',session_id='other',turn_id='later',input=json.dumps(dict(cmd='touch next-turn')))
            later=terminal(); assert later['status']=='completed',later
            assert (ws/'next-turn').exists()
            assert json.loads(db.execute("SELECT result FROM commands WHERE id='hung'").fetchone()[0])==saved
        if not fault:
            # Killing Rust must not release admission while its external command lives.
            for name in ('child-ready','parent-ready'): (ws/name).unlink()
            send(op='turn',request_id='crash',session_id='s',turn_id='crash',input=json.dumps(dict(cmd='python3 -c '+shlex.quote(code),yield_time_ms=200)))
            until=time.monotonic()+5
            while not (ws/'child-ready').exists() and time.monotonic()<until: time.sleep(.005)
            assert (ws/'child-ready').exists()
            # Stop the supervisor so EOF cannot immediately remove the evidence.
            crash_cg=P(P(f'/tmp/psi-external-command-{os.getuid()}.lock').read_text())
            supervisor=int((crash_cg.parent/'supervisor/cgroup.procs').read_text().strip())
            os.kill(supervisor,signal.SIGSTOP)
            try:
                p.kill();p.wait(timeout=5)
                p=subprocess.Popen([binary,'--state-dir',str(case/'state'),'--agent-loop','exec-fixture'],stdin=subprocess.PIPE,stdout=subprocess.PIPE,stderr=log,text=True)
                send(op='hello',capabilities=['sessions','origin-command-id','model-stream','durable-events']);read()
                send(op='open',request_id='reopen-other',session_id='other',workspace=str(ws));read()
                send(op='turn',request_id='after-restart',session_id='other',turn_id='after-restart',input=json.dumps(dict(cmd='touch forbidden-restart')))
                rejected=terminal()
                assert rejected['error'] in ('busy','external_cleanup_unconfirmed'),rejected
                assert not (ws/'forbidden-restart').exists()
                assert 'populated 1' in (crash_cg/'cgroup.events').read_text()
            finally:
                os.kill(supervisor,signal.SIGCONT)
                until=time.monotonic()+6
                while crash_cg.exists() and time.monotonic()<until:time.sleep(.01)
                assert not crash_cg.exists()
            restart_probe = dict(rejection=rejected,cgroup=str(crash_cg))
        db.close()
        reports.append(dict(fault=fault or None,elapsed_seconds=elapsed,pids=members,cgroup=str(cg),terminal=result,restart_probe=restart_probe))
    finally:
        p.stdin.close(); p.wait(timeout=10); log.close()
        # Wait for systemd's independent cleanup; never clear the external registry.
        until=time.monotonic()+8
        while 'cg' in locals() and cg.exists() and time.monotonic()<until: time.sleep(.05)
        assert 'cg' not in locals() or not cg.exists(),cg
(root/'report.json').write_text(json.dumps(reports,indent=2)+'\n')
print(json.dumps(dict(passed=True,root=str(root),cases=len(reports))))
