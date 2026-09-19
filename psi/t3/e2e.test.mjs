import assert from "node:assert/strict";
import { test } from "node:test";
import { spawn, execFileSync } from "node:child_process";
import { readFileSync, readlinkSync, writeFileSync } from "node:fs";
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

test("ordinary T3 browser message crosses WebSocket and Rust; viewer detach and controller cleanup", { timeout: 120_000 }, async () => {
  const port = Number(process.env.PSI_TEST_PORT ?? 8854);
  const webPort = port + 1;
  const launcher = spawn("python3", [resolve(here, "run.py"), checkout, "--port", String(port), "--web-port", String(webPort)], { stdio: ["ignore", "pipe", "pipe"] });
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
    await page.getByTestId("composer-editor").fill("Hello Psi issue 44");
    await page.getByTestId("composer-editor").press("Enter");
    await page.getByText("Psi fixture: Hello Psi issue 44", { exact: true }).waitFor();
    assert.equal(await page.getByText("Psi fixture: Hello Psi issue 44", { exact: true }).count(), 1);
    await page.screenshot({ path: resolve(root, "conversation.png"), fullPage: true });
    const sent = frames.find((frame) => frame.tag === "orchestration.dispatchCommand" && frame.payload?.type === "thread.turn.start");
    assert(sent, "normal composer must dispatch the ordinary WebSocket command");
    const db = new DatabaseSync(resolve(root, "userdata/state.sqlite"), { readOnly: true });
    try {
      const origin = db.prepare("SELECT command_id FROM orchestration_events WHERE event_type='thread.turn-start-requested' AND command_id=?").get(sent.payload.commandId);
      assert(origin, "originating command must exist in T3's durable event log");
      const messages = db.prepare("SELECT turn_id, text FROM projection_thread_messages WHERE role='assistant'").all();
      assert.deepEqual(messages.map((m) => ({ ...m })), [{ turn_id: `psi-turn:${origin.command_id}`, text: "Psi fixture: Hello Psi issue 44" }]);
    } finally { db.close(); }
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
    const report = { passed: true, t3: "dfbb11bdd7c3f1a5575cb55d3e3abb12be025727", node: process.version, chromium: execFileSync(executablePath, ["--version"], { encoding: "utf8" }).trim(), normal_websocket: true, visible_response: true, stable_origin_command_id: true, one_runtime: true, viewer_detach_preserves_runtime: true, controller_close_reaps_runtime: true };
    report.rustc = execFileSync("rustc", ["--version"], { encoding: "utf8" }).trim();
    report.cargo = execFileSync("cargo", ["--version"], { encoding: "utf8" }).trim();
    report.os = execFileSync("uname", ["-srmo"], { encoding: "utf8" }).trim();
    report.source_revision = execFileSync("git", ["rev-parse", "HEAD"], { cwd: here, encoding: "utf8" }).trim();
    report.implementation_sha256 = Object.fromEntries([
      "../Cargo.toml", "../Cargo.lock", "../src/core.rs", "../src/plugins.rs", "../src/protocol.rs", "../src/main.rs", "../src/lib.rs",
      "PsiDriver.ts", "PsiBridge.ts", "t3.patch", "prepare.py", "run.py", "e2e.test.mjs", "package-lock.json",
    ].map((name) => [name, createHash("sha256").update(readFileSync(resolve(here, name))).digest("hex")]));
    report.t3_lock_sha256 = createHash("sha256").update(readFileSync(resolve(checkout, "pnpm-lock.yaml"))).digest("hex");
    writeFileSync(resolve(root, "report.json"), JSON.stringify(report, null, 2) + "\n");
    console.log(`Evidence: ${root}/report.json`);
  } finally {
    if (browser) await browser.close();
    if (launcher.exitCode === null && launcher.signalCode === null) launcher.kill("SIGTERM");
    await closed;
    // Only terminate a PID whose ownership this test established.
    if (runtimePid && alive(runtimePid)) process.kill(runtimePid, "SIGKILL");
    if (root) writeFileSync(resolve(root, "test.log"), log.replace(/(pairingUrl: )\S+/g, "$1[redacted]"));
  }
});
