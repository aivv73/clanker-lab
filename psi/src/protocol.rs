//! Initial, non-durable JSONL controller. No tools, credentials, or file access.
use crate::{
    admission::TurnLease,
    plugins::LoopService,
    work::{Owner, Spawner, Work},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{collections::HashMap, sync::Arc};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

pub const VERSION: u32 = 1;
pub const CAPABILITIES: &[&str] = &["fixture-model", "sessions", "origin-command-id"];
const MAX_LINE: usize = 1024 * 1024;

#[derive(Deserialize, Serialize, Clone, PartialEq)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum Command {
    Hello {
        version: u32,
        capabilities: Vec<String>,
    },
    Open {
        version: u32,
        request_id: String,
        session_id: String,
    },
    Turn {
        version: u32,
        request_id: String,
        session_id: String,
        turn_id: String,
        input: String,
    },
    Close {
        version: u32,
        request_id: String,
        session_id: String,
    },
    Shutdown {
        version: u32,
        request_id: String,
    },
}
impl Command {
    fn version(&self) -> u32 {
        match self {
            Self::Hello { version, .. }
            | Self::Open { version, .. }
            | Self::Turn { version, .. }
            | Self::Close { version, .. }
            | Self::Shutdown { version, .. } => *version,
        }
    }
    fn request_id(&self) -> Option<&str> {
        match self {
            Self::Hello { .. } => None,
            Self::Open { request_id, .. }
            | Self::Turn { request_id, .. }
            | Self::Close { request_id, .. }
            | Self::Shutdown { request_id, .. } => Some(request_id),
        }
    }
}

pub struct Controller {
    runtime_id: String,
    ready: bool,
    agent: Arc<LoopService>,
    sessions: HashMap<String, Vec<Value>>,
    receipts: HashMap<String, (Command, Value)>,
    turns: HashMap<String, String>,
    work: Work,
    spawner: Spawner,
    active: Option<(Command, TurnLease)>,
    done_tx: tokio::sync::mpsc::UnboundedSender<(String, Result<String, String>)>,
    done_rx: tokio::sync::mpsc::UnboundedReceiver<(String, Result<String, String>)>,
    outbox: Vec<Value>,
}
impl Controller {
    pub fn new(agent: Arc<LoopService>, work: Work, spawner: Spawner) -> Self {
        let (done_tx, done_rx) = tokio::sync::mpsc::unbounded_channel();
        Self {
            runtime_id: uuid::Uuid::new_v4().to_string(),
            ready: false,
            agent,
            sessions: HashMap::new(),
            receipts: HashMap::new(),
            turns: HashMap::new(),
            work,
            spawner,
            active: None,
            done_tx,
            done_rx,
            outbox: Vec::new(),
        }
    }
    fn error(&self, code: &str) -> Value {
        json!({"version": VERSION, "runtime_id": self.runtime_id, "error": code})
    }
    async fn execute(&mut self, command: Command) -> Option<Value> {
        let request = command.request_id().map(str::to_owned);
        let reply = self.dispatch(command).await;
        reply.map(|mut reply| {
            if let Some(request) = request {
                reply["request_id"] = json!(request);
            }
            reply
        })
    }
    async fn dispatch(&mut self, command: Command) -> Option<Value> {
        if command.version() != VERSION {
            self.ready = false;
            return Some(self.error("incompatible_version"));
        }
        if let Command::Hello { capabilities, .. } = &command {
            if self.ready {
                return Some(self.error("already_initialized"));
            }
            if capabilities
                .iter()
                .any(|c| !CAPABILITIES.contains(&c.as_str()))
            {
                return Some(self.error("incompatible_capabilities"));
            }
            self.ready = true;
            return Some(
                json!({"version": VERSION, "runtime_id": self.runtime_id, "capabilities": CAPABILITIES, "model": "fixture", "pid": std::process::id()}),
            );
        }
        if !self.ready {
            return Some(self.error("handshake_required"));
        }
        let request_id = command.request_id().unwrap().to_owned();
        if request_id.trim().is_empty() {
            return Some(self.error("invalid_request_id"));
        }
        if let Some((original, reply)) = self.receipts.get(&request_id) {
            return Some(if original == &command {
                let mut reply = reply.clone();
                reply["duplicate"] = json!(true);
                reply
            } else {
                self.error("request_id_conflict")
            });
        }
        if let Some((pending, _)) = &self.active {
            if pending.request_id() == Some(&request_id) {
                return Some(self.error(if pending == &command {
                    "request_in_progress"
                } else {
                    "request_id_conflict"
                }));
            }
        }
        let mut reply = match &command {
            Command::Open { session_id, .. } => {
                if session_id.trim().is_empty() {
                    return Some(self.error("invalid_session_id"));
                }
                self.work.reopen_session(session_id);
                self.sessions.entry(session_id.clone()).or_default();
                json!({"session_id": session_id, "status": "ready"})
            }
            Command::Turn {
                session_id,
                turn_id,
                input,
                ..
            } => {
                if !self.sessions.contains_key(session_id) {
                    return Some(self.error("session_not_open"));
                }
                if turn_id.trim().is_empty() || self.turns.contains_key(turn_id) {
                    return Some(self.error("turn_id_conflict"));
                }
                if self.active.is_some() {
                    return Some(self.error("busy"));
                }
                let lease = match TurnLease::acquire() {
                    Ok(lease) => lease,
                    Err(error) => return Some(self.error(&error)),
                };
                let agent = self.agent.clone();
                let input = input.clone();
                let tx = self.done_tx.clone();
                let id = turn_id.clone();
                let owner = Owner::Turn {
                    session: session_id.clone(),
                    turn: turn_id.clone(),
                };
                if let Err(error) = self.spawner.spawn(owner, async move {
                    let result = agent.0.run(&input).await;
                    let _ = tx.send((id, result));
                }) {
                    return Some(self.error(&error));
                }
                self.turns.insert(turn_id.clone(), request_id.clone());
                eprintln!("Psi turn admitted: session={session_id} turn={turn_id}");
                self.active = Some((command, lease));
                return None;
            }

            Command::Close { session_id, .. } => {
                if self.active.as_ref().is_some_and(|(c, _)| matches!(c, Command::Turn { session_id: active, .. } if active == session_id)) {
                    let cancelled = self.finish(Err("session_closed".into())).await;
                    self.outbox.push(cancelled);
                }
                self.work
                    .close_owner(Owner::Session(session_id.clone()))
                    .await;
                self.sessions.remove(session_id);
                json!({"status": "closed", "session_id": session_id})
            }
            Command::Shutdown { .. } => {
                if self.active.is_some() {
                    let cancelled = self.finish(Err("runtime_closed".into())).await;
                    self.outbox.push(cancelled);
                }
                json!({"status": "shutdown"})
            }
            Command::Hello { .. } => unreachable!(),
        };
        reply["version"] = json!(VERSION);
        reply["runtime_id"] = json!(self.runtime_id);
        reply["request_id"] = json!(request_id);
        self.receipts.insert(request_id, (command, reply.clone()));
        Some(reply)
    }
    async fn finish(&mut self, result: Result<String, String>) -> Value {
        let (command, _lease) = self.active.take().unwrap();
        let Command::Turn {
            session_id,
            turn_id,
            request_id,
            input,
            ..
        } = &command
        else {
            unreachable!()
        };
        self.work
            .close_owner(Owner::Turn {
                session: session_id.clone(),
                turn: turn_id.clone(),
            })
            .await;
        let mut reply = json!({"version": VERSION, "runtime_id": self.runtime_id, "request_id": request_id, "session_id": session_id, "turn_id": turn_id});
        match result {
            Ok(output) => {
                reply["status"] = json!("completed");
                reply["output"] = json!(output);
                reply["input"] = json!(input);
                self.sessions
                    .get_mut(session_id)
                    .unwrap()
                    .push(reply.clone());
            }
            Err(reason) => {
                reply["status"] = json!(if reason == "model_failed" {
                    "failed"
                } else {
                    "cancelled"
                });
                reply["error"] = json!(reason);
            }
        }
        self.receipts
            .insert(request_id.clone(), (command, reply.clone()));
        eprintln!("Psi turn cleaned: tasks={}", self.work.count());
        reply
    }
    pub async fn serve(&mut self) -> Result<(), String> {
        let result = self.serve_inner().await;
        if self.active.is_some() {
            self.finish(Err("controller_closed".into())).await;
        }
        result
    }
    async fn serve_inner(&mut self) -> Result<(), String> {
        let mut input = BufReader::new(tokio::io::stdin());
        let mut output = tokio::io::stdout();
        let mut line = Vec::new();
        loop {
            let reply = tokio::select! {
                frame = read_frame(&mut input, &mut line) => {
                    if !frame? { break; }
                    let parsed = serde_json::from_slice::<Command>(&line);
                    line.clear();
                    match parsed {
                        Ok(command) => self.execute(command).await,
                        Err(_) => Some(self.error("invalid_command")),
                    }
                }
                Some((id, result)) = self.done_rx.recv() => {
                    if self.active.as_ref().is_some_and(|(c, _)| matches!(c, Command::Turn { turn_id, .. } if turn_id == &id)) {
                        Some(self.finish(result.map_err(|_| "model_failed".to_string())).await)
                    } else { None }
                }
            };
            let stop = reply.as_ref().is_some_and(|reply| {
                reply["status"] == "shutdown"
                    || matches!(
                        reply["error"].as_str(),
                        Some("incompatible_version" | "incompatible_capabilities")
                    )
            });
            self.outbox.extend(reply);
            for reply in self.outbox.drain(..) {
                let mut bytes = serde_json::to_vec(&reply).map_err(|e| e.to_string())?;
                bytes.push(b'\n');
                output.write_all(&bytes).await.map_err(|e| e.to_string())?;
                output.flush().await.map_err(|e| e.to_string())?;
            }
            if stop {
                break;
            }
        }
        Ok(())
    }
}

// Partial frame bytes survive cancellation when a completed turn wins select!.
async fn read_frame(
    input: &mut BufReader<tokio::io::Stdin>,
    line: &mut Vec<u8>,
) -> Result<bool, String> {
    loop {
        let buffer = input.fill_buf().await.map_err(|e| e.to_string())?;
        if buffer.is_empty() {
            return if line.is_empty() {
                Ok(false)
            } else {
                Err("incomplete JSONL frame".into())
            };
        }
        let count = buffer
            .iter()
            .position(|b| *b == b'\n')
            .map_or(buffer.len(), |i| i + 1);
        if line.len() + count > MAX_LINE {
            return Err("protocol line exceeds 1 MiB".into());
        }
        line.extend_from_slice(&buffer[..count]);
        input.consume(count);
        if line.last() == Some(&b'\n') {
            return Ok(true);
        }
    }
}
