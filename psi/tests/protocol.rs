use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

struct Process {
    child: Child,
    input: Option<ChildStdin>,
    output: BufReader<ChildStdout>,
}
impl Process {
    fn new() -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_psi"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        Self {
            input: child.stdin.take(),
            output: BufReader::new(child.stdout.take().unwrap()),
            child,
        }
    }
    fn send(&mut self, command: Value) -> Value {
        writeln!(self.input.as_mut().unwrap(), "{command}").unwrap();
        loop {
            let mut line = String::new();
            self.output.read_line(&mut line).unwrap();
            let reply: Value = serde_json::from_str(&line).unwrap();
            if reply["status"] != "streaming" {
                return reply;
            }
        }
    }
    fn hello(&mut self) -> Value {
        self.send(json!({"op":"hello","version":1,"capabilities":["fixture-model","sessions","origin-command-id","model-stream"]}))
    }
}
impl Drop for Process {
    fn drop(&mut self) {
        self.input.take();
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn real_process_conversation_identity_and_eof() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let mut p = Process::new();
    let hello = p.hello();
    assert_eq!(hello["pid"], p.child.id());
    for session in ["s1", "s2"] {
        assert_eq!(p.send(json!({"op":"open","version":1,"request_id":format!("open:{session}"),"session_id":session}))["status"], "ready");
    }
    let turn = json!({"op":"turn","version":1,"request_id":"origin:1","session_id":"s1","turn_id":"turn:1","input":"Привет\nPsi"});
    let reply = p.send(turn.clone());
    assert_eq!(reply["output"], "Psi fixture: Привет\nPsi");
    assert_eq!(reply["runtime_id"], hello["runtime_id"]);
    assert_eq!(reply["request_id"], "origin:1");
    assert_eq!(p.send(turn.clone())["duplicate"], true);
    let mut conflict = turn;
    conflict["input"] = json!("different");
    assert_eq!(p.send(conflict)["error"], "request_id_conflict");
    p.send(json!({"op":"close","version":1,"request_id":"close:1","session_id":"s1"}));
    assert_eq!(p.send(json!({"op":"turn","version":1,"request_id":"origin:2","session_id":"s2","turn_id":"turn:2","input":"second"}))["output"], "Psi fixture: second");
    p.input.take();
    assert!(p.child.wait().unwrap().success());
    let mut rest = String::new();
    assert_eq!(p.output.read_line(&mut rest).unwrap(), 0);
}

#[test]
fn rejects_before_handshake_and_incompatible_peers() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let mut p = Process::new();
    assert_eq!(p.send(json!({"op":"turn","version":1,"request_id":"1","session_id":"s","turn_id":"t","input":"hi"}))["error"], "handshake_required");
    assert_eq!(
        p.send(json!({"op":"hello","version":2,"capabilities":[]}))["error"],
        "incompatible_version"
    );
    assert!(p.child.wait().unwrap().success());
    let mut p = Process::new();
    assert_eq!(
        p.send(json!({"op":"hello","version":1,"capabilities":["real-tools"]}))["error"],
        "incompatible_capabilities"
    );
    assert!(p.child.wait().unwrap().success());
}

#[test]
fn malformed_command_and_turn_identity_are_explicit() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let mut p = Process::new();
    p.hello();
    assert_eq!(
        p.send(json!({"op":"nonsense","version":1}))["error"],
        "invalid_command"
    );
    p.send(json!({"op":"open","version":1,"request_id":"o","session_id":"s"}));
    let mut turn = json!({"op":"turn","version":1,"request_id":"a","session_id":"s","turn_id":"t","input":"same"});
    assert_eq!(p.send(turn.clone())["status"], "completed");
    turn["request_id"] = json!("b");
    assert_eq!(p.send(turn)["error"], "turn_id_conflict");
}
