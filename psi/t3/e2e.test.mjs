import assert from "node:assert/strict";
import { test } from "node:test";
import { spawn, execFileSync } from "node:child_process";
import { readFileSync, readlinkSync, writeFileSync, symlinkSync, existsSync } from "node:fs";
import { createServer } from "node:net";
import { once } from "node:events";
import { fileURLToPath } from "node:url";
import { resolve, dirname } from "node:path";
import { DatabaseSync } from "node:sqlite";
import { createHash } from "node:crypto";
import { chromium } from "playwright-core";

const here = dirname(fileURLToPath(import.meta.url));
const checkout = process.env.PSI_T3_CHECKOUT;
const executablePath = process.env.PSI_CHROMIUM;
assert(checkout, "PSI_T3_CHECKOUT must name the disposable pinned T3 checkout");
assert(executablePath, "PSI_CHROMIUM must name a Chromium executable");

function alive(pid) {
  try { process.kill(pid, 0); return true; } catch (error) { if (error.code === "ESRCH") return false; throw error; }
}

function recordVersions(report) {
    report.psi_binary_sha256 = createHash("sha256").update(readFileSync(resolve(here,"../target/debug/psi"))).digest("hex");
    report.rustc = execFileSync("rustc", ["--version"], { encoding: "utf8" }).trim();
    report.cargo = execFileSync("cargo", ["--version"], { encoding: "utf8" }).trim();
    report.os = execFileSync("uname", ["-srmo"], { encoding: "utf8" }).trim();
    report.source_revision = execFileSync("git", ["rev-parse", "HEAD"], { cwd: here, encoding: "utf8" }).trim();
    report.implementation_sha256 = Object.fromEntries([
      "../src/executor.rs", "../tests/executor.rs", "../tests/fixtures/executor.py", "../src/policy.rs", "../tests/policy.rs", "../tests/fixtures/policy.py", "../policy/build.py", "../policy/Policy.bend", "../policy/LAWS.bend", "../policy/PROOF.bend", "../policy/Main.bend", "../src/store.rs", "../tests/durability.rs", "../tests/fixtures/durability.py", "PsiInbox.ts", "PsiProjection.ts", "inbox.test.ts", "durability-worker.ts", "../Cargo.toml", "../Cargo.lock", "../src/admission.rs", "../src/work.rs", "../src/core.rs", "../src/plugins.rs", "../src/protocol.rs", "../src/main.rs", "../src/lib.rs", "../src/codex.rs", "../src/model.rs", "../tests/codex.rs", "../tests/auth_runtime.rs", "../tests/fixtures/runtime_auth.py", "../tests/fixtures/oauth_server.py", "../tests/lifecycle.rs", "../tests/protocol.rs",
      "PsiWorkspace.ts", "workspace.test.ts", "PsiDriver.ts", "PsiBridge.ts", "adapter.test.ts", "t3.patch", "prepare.py", "run.py", "e2e.test.mjs", "package-lock.json",
    ].map((name) => [name, createHash("sha256").update(readFileSync(resolve(here, name))).digest("hex")]));
    report.t3_lock_sha256 = createHash("sha256").update(readFileSync(resolve(checkout, "pnpm-lock.yaml"))).digest("hex");
}

test("ordinary T3 browser message crosses WebSocket and Rust; viewer detach and controller cleanup", { timeout: 120_000 }, async () => {
  const port = Number(process.env.PSI_TEST_PORT ?? 8854);
  const webPort = port + 1;
  const exec = process.env.PSI_TEST_EXEC === "1";
  const listeners = [];
  let connections = 0;
  const policy = process.env.PSI_TEST_POLICY;
  const policyCancel = policy === "cancel";
  const policyFault = policy && !["normal", "cancel"].includes(policy) ? policy : undefined;
  const agentLoop = exec ? "exec-fixture" : policy ? "policy-fixture" : process.env.PSI_TEST_LOOP ?? "single-response";
  const fault = process.env.PSI_TEST_STARTUP_FAULT;
  const lifecycle = process.env.PSI_TEST_LIFECYCLE === "1";
  const options = ["--agent-loop", agentLoop];
  const oauth = Boolean(process.env.PSI_TEST_OAUTH);
  const cancelStream = process.env.PSI_TEST_OAUTH === "hold";
  let fixture;
  if (oauth) {
    fixture = spawn("python3", [resolve(here, "../tests/fixtures/oauth_server.py"), cancelStream ? "hold" : "normal"], { stdio: ["ignore", "pipe", "inherit"] });
    const [chunk] = await once(fixture.stdout, "data");
    options.push("--model", "fixture-codex", "--oauth-fixture", chunk.toString().trim());
  }
  if (policyCancel) options.push("--hold-policy");
  if (policyFault) options.push("--policy-fault", policyFault);
  const policyErrors = {missing:"policy_unavailable", hang:"policy_timeout", invalid:"policy_invalid_response", mismatch:"policy_identity_mismatch", failed:"policy_process_failed"};
  const expected = exec ? "exec_command: exit\nisolated command complete" : policyCancel ? "Cancelled. Policy admission is closed." : policyFault ? `Psi failed: ${policyErrors[policyFault]}` : policy ? "Psi policy fixture: counter=1; admitted by Bend." : oauth ? "Psi HTTP fixture: Привет 🌍" : "Psi fixture: Hello Psi issue 44";
  if (fault) options.push("--startup-fault", fault);
  if (lifecycle) options.push("--hold-fixture");
  const launcher = spawn("python3", [resolve(here, "run.py"), checkout, "--port", String(port), "--web-port", String(webPort), ...options], { stdio: ["ignore", "pipe", "pipe"] });
  const closed = once(launcher, "close");
  let log = "";
  let root;
  let browser;
  let runtimePid;
  let serverPid;
  const pageErrors = [];
  const frames = [];
  const startup = new Promise((resolve, reject) => {
    const timer = setTimeout(() => reject(new Error("T3 startup timed out; see test log")), 60_000);
    function inspect(data) {
      log += data.toString();
      root = log.match(/Psi scratch root: (.+)/)?.[1];
      if (root && log.includes("pairingUrl:") && log.includes(`127.0.0.1:${webPort}/`)) {
        clearTimeout(timer); resolve();
      }
    }
    launcher.stdout.on("data", inspect);
    launcher.stderr.on("data", inspect);
    launcher.on("error", (error) => { clearTimeout(timer); reject(error); });
    launcher.on("exit", (code) => { clearTimeout(timer); reject(new Error(`T3 launcher exited: ${code}`)); });
  });
  try {
    await startup;
    serverPid = JSON.parse(readFileSync(resolve(root, "run.json"), "utf8")).server_pid;
    browser = await chromium.launch({ executablePath, headless: true, args: ["--no-sandbox"] });
    const page = await browser.newPage();
    // Retain the real application's socket only to send ordinary lifecycle RPCs.
    await page.addInitScript(() => {
      const send = WebSocket.prototype.send;
      WebSocket.prototype.send = function(data) { window.psiSocket = this; return send.call(this, data); };
    });
    page.on("pageerror", (error) => pageErrors.push(error.message));
    page.on("websocket", (socket) => {
      socket.on("framesent", ({ payload }) => { try { frames.push(JSON.parse(String(payload))); } catch {} });
    });
    const pairing = log.match(/pairingUrl: (\S+)/)[1].replace(`:${port}`, `:${webPort}`);
    await page.goto(pairing);
    await page.getByRole("button", { name: "Continue", exact: true }).click({ timeout: 45_000 });
    await page.getByRole("button", { name: "Continue", exact: true }).click();
    await page.getByRole("button", { name: "Do not import projects", exact: true }).click();
    await page.getByRole("button", { name: "New thread", exact: true }).first().click();
    if (exec) {
      writeFileSync(resolve(root,"workspace/target.txt"),"value=old\n");
      writeFileSync(resolve(root,"outside-control"),"unchanged");
      symlinkSync(resolve(root,"outside-control"),resolve(root,"workspace/escape"));
      const unix = createServer(socket=>{connections++;socket.destroy();});
      const tcp = createServer(socket=>{connections++;socket.destroy();});
      listeners.push(unix,tcp);
      await new Promise(r=>unix.listen(resolve(root,"workspace/host.sock"),r));
      await new Promise(r=>tcp.listen(0,"127.0.0.1",r));
      writeFileSync(resolve(root,"workspace/isolation.py"),`
import socket,pathlib,ctypes,errno,os,stat
for path in ['escape','/home','/sys/fs/cgroup','/run/user']:
 try: pathlib.Path(path).read_bytes(); raise AssertionError(path)
 except OSError: pass
try: pathlib.Path('/usr/psi-control').write_text('bad'); raise AssertionError('system write')
except OSError: pass
try: pathlib.Path('.git/psi-write').write_text('bad'); raise AssertionError('Git metadata write')
except OSError: pass
for fn in [lambda:socket.socket(socket.AF_UNIX),socket.socketpair,lambda:socket.create_connection(('127.0.0.1',${tcp.address().port}),.1)]:
 try: fn(); raise AssertionError('host IPC')
 except OSError: pass
lib=ctypes.CDLL(None,use_errno=True)
assert lib.syscall(425,1,0)==-1 and ctypes.get_errno()==errno.EPERM
assert all(not stat.S_ISSOCK(os.fstat(fd).st_mode) for fd in (0,1,2))
print('isolated command complete',end='')
`);
    }
    await page.getByTestId("composer-editor").fill(exec ? JSON.stringify({cmd:"printf 'value=new\\n' > target.txt; python3 isolation.py"}) : "Hello Psi issue 44");
    await page.getByTestId("composer-editor").press("Enter");
    if (oauth) {
      const signIn = page.getByRole("link", { name: /127\.0\.0\.1.*authorize/ });
      await signIn.waitFor();
      const url = await signIn.getAttribute("href");
      assert(url);
      const authPage = await browser.newPage();
      await authPage.goto(url);
      await authPage.getByText("Sign-in received. Return to Psi.").waitFor();
      await authPage.close();
    }
    if (fault) {
      await page.getByText(/Psi startup failed/).first().waitFor();
      assert(!log.includes("Psi runtime started"));
      if (fault === "partial") {
        assert(log.includes("probe resource dropped"));
        assert(log.includes("registrations=0 tasks=0"));
      } else assert(!log.includes("Psi plugin starting"));
      const db = new DatabaseSync(resolve(root, "userdata/state.sqlite"), { readOnly: true });
      try {
        assert.equal(db.prepare("SELECT COUNT(*) AS n FROM projection_thread_messages WHERE role='assistant'").get().n, 0);
        const errors = db.prepare("SELECT payload_json FROM orchestration_events WHERE event_type='thread.session-set'").all().map((row) => JSON.parse(row.payload_json)).filter((row) => row.session.status === "error");
        assert(errors.some((row) => row.session.lastError.includes("Psi startup failed")));
      } finally { db.close(); }
      const children = readFileSync(`/proc/${serverPid}/task/${serverPid}/children`, "utf8").trim().split(/\s+/).filter(Boolean);
      assert(!children.some((pid) => { try { return readlinkSync(`/proc/${pid}/exe`) === resolve(here, "../target/debug/psi"); } catch { return false; } }));
      await page.screenshot({ path: resolve(root, "startup-failure.png"), fullPage: true });
      const report = { passed: true, startup_fault: fault, readiness: false, node: process.version, agent_loop: agentLoop, t3: "dfbb11bdd7c3f1a5575cb55d3e3abb12be025727" };
      recordVersions(report);
      writeFileSync(resolve(root, "report.json"), JSON.stringify(report, null, 2));
      console.log(`Evidence: ${root}/report.json`);
      return;
    }
    if (lifecycle) {
      // Wait for Rust admission, not an arbitrary delay, before the conflict.
      for (let n = 0; !log.includes("Psi turn admitted:"); n++) {
        assert(n < 400, "held turn did not reach Rust");
        await new Promise((resolve) => setTimeout(resolve, 25));
      }
      const first = frames.find((frame) => frame.tag === "orchestration.dispatchCommand" && frame.payload?.type === "thread.turn.start");
      assert(first);
      await page.getByRole("button", { name: "New thread", exact: true }).first().click();
      await page.getByTestId("composer-editor").fill("must not queue");
      await page.getByTestId("composer-editor").press("Enter");
      await page.getByText(/Psi: busy/).first().waitFor();
      assert.equal((log.match(/Psi turn admitted:/g) ?? []).length, 1);
      for (let i = 0; i < 2; i++) {
        await page.evaluate(({ frame, i }) => window.psiSocket.send(JSON.stringify({ ...frame, id: String(900000 + i), payload: { type: "thread.session.stop", commandId: `psi-close-${i}`, threadId: frame.payload.threadId, createdAt: new Date().toISOString() } })), { frame: first, i });
      }
      for (let n = 0; !log.includes("Psi turn cleaned: tasks=0"); n++) {
        assert(n < 400, "session close failed to clean held work");
        await new Promise((resolve) => setTimeout(resolve, 25));
      }
      assert(!log.includes("Psi plugin closed:"), "session close must preserve shared plugins");
      writeFileSync(resolve(root, "release-fixture"), "release");
      await page.getByTestId("composer-editor").fill("Hello Psi issue 44");
    await page.getByTestId("composer-editor").press("Enter");
    }
    if (policyCancel) {
      await page.getByText("Psi policy: Permit received; waiting for fixture release.").waitFor();
      await page.getByRole("button", {name:"Stop generation", exact:true}).click();
      assert(frames.some(frame => frame.payload?.type === "thread.turn.interrupt"));
    }
    await page.getByText(expected, { exact: !exec && !oauth && !policyCancel && !policyFault }).waitFor();
    assert.equal(await page.getByText(expected, { exact: !exec && !oauth && !policyCancel && !policyFault }).count(), 1);
    if (cancelStream) {
      await page.getByRole("button", { name: "Stop generation", exact: true }).click();
      await page.getByText(/Cancelled locally\. Server processing and billing may continue\./).waitFor();
      assert(frames.some((frame) => frame.payload?.type === "thread.turn.interrupt"));
    }
    await page.screenshot({ path: resolve(root, "conversation.png"), fullPage: true });
    const sent = frames.findLast((frame) => frame.tag === "orchestration.dispatchCommand" && frame.payload?.type === "thread.turn.start");
    assert(sent, "normal composer must dispatch the ordinary WebSocket command");
    const db = new DatabaseSync(resolve(root, "userdata/state.sqlite"), { readOnly: true });
    try {
      const origin = db.prepare("SELECT command_id FROM orchestration_events WHERE event_type='thread.turn-start-requested' AND command_id=?").get(sent.payload.commandId);
      assert(origin, "originating command must exist in T3's durable event log");
      const messages = db.prepare("SELECT turn_id, text FROM projection_thread_messages WHERE role='assistant'").all();
      assert.equal(messages.length, oauth ? 2 : 1);
      assert(messages.every((message) => message.turn_id === `psi-turn:${origin.command_id}`));
      const answer = messages.find((message) => message.text.includes(expected));
      assert(answer);
      assert(cancelStream ? answer.text.includes("Cancelled locally. Server processing and billing may continue.") : policyFault ? answer.text.includes(expected) : answer.text.endsWith(expected));
      assert(!JSON.stringify(messages).includes("synthetic-refresh"));
    } finally { db.close(); }
    if (exec) {
      assert.equal(connections,0,"independent host listeners must receive no connection");
      assert.equal(readFileSync(resolve(root,"outside-control"),"utf8"),"unchanged");
      assert.equal(await page.getByRole("button",{name:"Edit from here",exact:true}).count(),0,"rollback is absent from UI");
      assert.equal(readFileSync(resolve(root,"workspace/target.txt"),"utf8"),"value=new\n");
      const database = new DatabaseSync(resolve(root,"userdata/state.sqlite"), {readOnly:true});
      try {
        let checkpoint;
        for (let i=0;i<100;i++) {
          checkpoint=database.prepare("SELECT * FROM projection_turns WHERE checkpoint_ref IS NOT NULL").all().find(r=>r.turn_id===`psi-turn:${sent.payload.commandId}`);
          if(checkpoint) break;
          await new Promise(r=>setTimeout(r,50));
        }
        assert(checkpoint,"terminal checkpoint must be captured");
        const refs=execFileSync("git",["for-each-ref","--format=%(refname)","refs/t3"],{cwd:resolve(root,"workspace"),encoding:"utf8"});
        const baseline=refs.trim().split("\n").find(r=>r.endsWith("/0"));
        assert(baseline,refs);
        assert.equal(execFileSync("git",["show",`${baseline}:target.txt`],{cwd:resolve(root,"workspace"),encoding:"utf8"}),"value=old\n");
        const diff=execFileSync("git",["diff",baseline,checkpoint.checkpoint_ref,"--","target.txt"],{cwd:resolve(root,"workspace"),encoding:"utf8"});
        assert(diff.includes("-value=old") && diff.includes("+value=new"),diff);
        await page.locator('[data-changed-files-state="tree"]').getByText("target.txt",{exact:true}).waitFor();
        await page.getByRole("button",{name:"Open diff",exact:true}).click();
        await page.getByRole("button",{name:"Expand target.txt",exact:true}).click();
        try { await page.getByText(/value=old/).first().waitFor({timeout:10000}); }
        catch (error) { writeFileSync(resolve(root,"diff-page.html"),await page.content()); await page.screenshot({path:resolve(root,"diff-failure.png"),fullPage:true}); throw error; }
        await page.screenshot({path:resolve(root,"first-diff.png"),fullPage:true});
        const before=JSON.stringify(database.prepare("SELECT * FROM projection_thread_messages ORDER BY rowid").all());
        for (const [index,type] of ["thread.checkpoint.revert","thread.conversation.revert"].entries()) {
          const commandId=`psi-rollback-probe-${index}`;
          await page.evaluate(({frame,type,commandId,index})=>window.psiSocket.send(JSON.stringify({...frame,id:String(900003+index),payload:{type,commandId,threadId:frame.payload.threadId,turnCount:0,createdAt:new Date().toISOString()}})),{frame:sent,type,commandId,index});
          let rejection;
          for(let i=0;i<100;i++) {
            rejection=database.prepare("SELECT status,error FROM orchestration_command_receipts WHERE command_id=?").get(commandId);
            if(rejection)break;
            await new Promise(r=>setTimeout(r,25));
          }
          assert.equal(rejection?.status,"rejected");
          assert(rejection.error.includes("Psi rollback is unsupported"),rejection.error);
        }
        assert.equal(readFileSync(resolve(root,"workspace/target.txt"),"utf8"),"value=new\n");
        assert.equal(JSON.stringify(database.prepare("SELECT * FROM projection_thread_messages ORDER BY rowid").all()),before);
        assert.equal(execFileSync("git",["for-each-ref","--format=%(refname)","refs/t3"],{cwd:resolve(root,"workspace"),encoding:"utf8"}),refs);
      } finally { database.close(); }
      await page.getByTestId("composer-editor").fill(JSON.stringify({cmd:"touch /workspace/must-not-run",workdir:"/workspace/absent"}));
      await page.getByTestId("composer-editor").press("Enter");
      await page.getByText(/exec_command: launch_failure/).waitFor();
      assert(!existsSync(resolve(root,"workspace/must-not-run")),"preflight failure must not run the command");
      assert.equal(readFileSync(resolve(root,"workspace/target.txt"),"utf8"),"value=new\n");
      await page.screenshot({path:resolve(root,"exec-and-preflight.png"),fullPage:true});
    }
    let policyEvidence;
    if (policy) {
      const state = resolve(root,"psi",createHash("sha256").update("psi").digest("hex"),"history.sqlite");
      const runtime = new DatabaseSync(state,{readOnly:true});
      try {
        const rows = runtime.prepare("SELECT result FROM commands WHERE turn IS NOT NULL").all();
        assert.equal(rows.length,1);
        const terminal = JSON.parse(rows[0].result);
        policyEvidence = terminal.policy;
        assert.equal(policyEvidence.handoffs,policy === "normal" ? 1 : 0);
        assert.equal(policyEvidence.closed,true);
        assert.equal(terminal.status,policyCancel ? "cancelled" : policyFault ? "failed" : "completed");
        if (!policyFault) assert.equal(policyEvidence.state,policyCancel ? "Cancelled" : "Finished");
        if (policyCancel) {
          writeFileSync(resolve(root,"release-policy"),"late release");
          await new Promise(resolve => setTimeout(resolve,100));
          assert.equal(runtime.prepare("SELECT COUNT(*) AS n FROM commands WHERE turn IS NOT NULL").get().n,1);
        }
      } finally {runtime.close();}
      const policyPids = [...log.matchAll(/Psi policy child: (\d+)/g)].map(match=>Number(match[1]));
      if (policy !== "missing") assert(policyPids.length > 0);
      for (const pid of policyPids) { assert(!alive(pid)); assert(log.includes(`Psi policy reaped: ${pid}`)); }
    }
    const children = readFileSync(`/proc/${serverPid}/task/${serverPid}/children`, "utf8").trim().split(/\s+/).filter(Boolean).map(Number);
    const runtimes = children.filter((pid) => {
      try { return readlinkSync(`/proc/${pid}/exe`) === resolve(here, "../target/debug/psi"); } catch { return false; }
    });
    assert.equal(runtimes.length, 1, "exactly one standalone Psi child belongs to the T3 server");
    runtimePid = runtimes[0];
    // Viewer lifetime must not own the provider process.
    await page.goto("about:blank");
    assert(alive(runtimePid));
    await browser.close(); browser = undefined;
    launcher.kill("SIGTERM");
    await closed;
    assert(!alive(runtimePid), "Rust must be reaped after the controller server closes");
    assert(log.includes("Psi runtime stopped"), "EOF must run Rust lifecycle cleanup");
    assert.deepEqual(pageErrors, []);
    const report = { passed: true, exec_fixture: exec, policy_fixture: policy, policy_evidence: policyEvidence, policy_build: (policy || exec) ? JSON.parse(readFileSync(resolve(here,"../policy/.build/manifest.json"),"utf8")) : undefined, oauth_fixture: oauth, local_stream_cancellation: cancelStream, agent_loop: agentLoop, lifecycle, t3: "dfbb11bdd7c3f1a5575cb55d3e3abb12be025727", node: process.version, chromium: execFileSync(executablePath, ["--version"], { encoding: "utf8" }).trim(), normal_websocket: true, visible_response: true, stable_origin_command_id: true, one_runtime: true, viewer_detach_preserves_runtime: true, controller_close_reaps_runtime: true };
    recordVersions(report);
    writeFileSync(resolve(root, "report.json"), JSON.stringify(report, null, 2) + "\n");
    console.log(`Evidence: ${root}/report.json`);
  } finally {
    for (const listener of listeners) listener.close();
    if (fixture) fixture.kill("SIGTERM");
    if (browser) await browser.close();
    if (launcher.exitCode === null && launcher.signalCode === null) launcher.kill("SIGTERM");
    await closed;
    // Only terminate a PID whose ownership this test established.
    if (runtimePid && alive(runtimePid)) process.kill(runtimePid, "SIGKILL");
    if (root) writeFileSync(resolve(root, "test.log"), log.replace(/(pairingUrl: )\S+/g, "$1[redacted]"));
  }
});
