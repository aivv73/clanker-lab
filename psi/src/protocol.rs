//! Initial, non-durable JSONL controller. No tools, credentials, or file access.
use crate::plugins::LoopService;
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
}
impl Controller {
    pub fn new(agent: Arc<LoopService>) -> Self {
        Self {
            runtime_id: uuid::Uuid::new_v4().to_string(),
            ready: false,
            agent,
            sessions: HashMap::new(),
            receipts: HashMap::new(),
            turns: HashMap::new(),
        }
    }
    fn error(&self, code: &str) -> Value {
        json!({"version": VERSION, "runtime_id": self.runtime_id, "error": code})
    }
    pub async fn execute(&mut self, command: Command) -> Value {
        if command.version() != VERSION {
            self.ready = false;
            return self.error("incompatible_version");
        }
        if let Command::Hello { capabilities, .. } = &command {
            if self.ready {
                return self.error("already_initialized");
            }
            if capabilities
                .iter()
                .any(|c| !CAPABILITIES.contains(&c.as_str()))
            {
                return self.error("incompatible_capabilities");
            }
            self.ready = true;
            return json!({"version": VERSION, "runtime_id": self.runtime_id, "capabilities": CAPABILITIES, "model": "fixture", "pid": std::process::id()});
        }
        if !self.ready {
            return self.error("handshake_required");
        }
        let request_id = command.request_id().unwrap().to_owned();
        if request_id.trim().is_empty() {
            return self.error("invalid_request_id");
        }
        if let Some((original, reply)) = self.receipts.get(&request_id) {
            return if original == &command {
                let mut reply = reply.clone();
                reply["duplicate"] = json!(true);
                reply
            } else {
                self.error("request_id_conflict")
            };
        }
        let mut reply = match &command {
            Command::Open { session_id, .. } => {
                if session_id.trim().is_empty() {
                    return self.error("invalid_session_id");
                }
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
                    return self.error("session_not_open");
                }
                if turn_id.trim().is_empty() || self.turns.contains_key(turn_id) {
                    return self.error("turn_id_conflict");
                }
                self.turns.insert(turn_id.clone(), request_id.clone());
                match self.agent.0.run(input).await {
                    Ok(output) => {
                        let turn = json!({"session_id": session_id, "turn_id": turn_id, "input": input, "output": output, "status": "completed"});
                        self.sessions
                            .get_mut(session_id)
                            .unwrap()
                            .push(turn.clone());
                        turn
                    }
                    Err(_) => {
                        json!({"session_id": session_id, "turn_id": turn_id, "status": "failed", "error": "model_failed"})
                    }
                }
            }
            Command::Close { session_id, .. } => {
                self.sessions.remove(session_id);
                json!({"status": "closed", "session_id": session_id})
            }
            Command::Shutdown { .. } => json!({"status": "shutdown"}),
            Command::Hello { .. } => unreachable!(),
        };
        reply["version"] = json!(VERSION);
        reply["runtime_id"] = json!(self.runtime_id);
        reply["request_id"] = json!(request_id);
        self.receipts.insert(request_id, (command, reply.clone()));
        reply
    }
    pub async fn serve(&mut self) -> Result<(), String> {
        let mut input = BufReader::new(tokio::io::stdin());
        let mut output = tokio::io::stdout();
        loop {
            // Bounded framing, including input without a newline.
            let mut line = Vec::new();
            loop {
                let buffer = input.fill_buf().await.map_err(|e| e.to_string())?;
                if buffer.is_empty() {
                    break;
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
                    break;
                }
            }
            if line.is_empty() {
                break;
            }
            if line.last() != Some(&b'\n') {
                return Err("incomplete JSONL frame".into());
            }
            let reply = match serde_json::from_slice::<Command>(&line) {
                Ok(command) => self.execute(command).await,
                Err(_) => self.error("invalid_command"),
            };
            let stop = reply["status"] == "shutdown"
                || matches!(
                    reply["error"].as_str(),
                    Some("incompatible_version" | "incompatible_capabilities")
                );
            let mut bytes = serde_json::to_vec(&reply).map_err(|e| e.to_string())?;
            bytes.push(b'\n');
            output.write_all(&bytes).await.map_err(|e| e.to_string())?;
            output.flush().await.map_err(|e| e.to_string())?;
            if stop {
                break;
            }
        }
        Ok(())
    }
}
