//! Durable JSONL controller. Credentials stay behind the model service boundary.
use crate::{
    admission::TurnLease,
    model::{ModelEvent, ModelRequest, ModelResponse},
    plugins::LoopService,
    work::{Owner, Spawner, Work},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{collections::HashMap, sync::Arc};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

pub const VERSION: u32 = 1;
pub const CAPABILITIES: &[&str] = &[
    "sessions",
    "origin-command-id",
    "model-stream",
    "memory-oauth",
    "durable-events",
];
type DurableResult<T> = std::result::Result<T, String>;
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
        workspace: Option<String>,
    },
    Replay {
        version: u32,
        request_id: String,
        session_id: String,
        after: u64,
    },
    History {
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
    Cancel {
        version: u32,
        request_id: String,
        session_id: String,
        turn_id: Option<String>,
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
            | Self::Replay { version, .. }
            | Self::History { version, .. }
            | Self::Open { version, .. }
            | Self::Turn { version, .. }
            | Self::Cancel { version, .. }
            | Self::Close { version, .. }
            | Self::Shutdown { version, .. } => *version,
        }
    }
    fn request_id(&self) -> Option<&str> {
        match self {
            Self::Hello { .. } => None,
            Self::Replay { request_id, .. }
            | Self::History { request_id, .. }
            | Self::Open { request_id, .. }
            | Self::Turn { request_id, .. }
            | Self::Cancel { request_id, .. }
            | Self::Close { request_id, .. }
            | Self::Shutdown { request_id, .. } => Some(request_id),
        }
    }
}

pub struct Controller {
    policy: Option<Arc<crate::policy::PolicyService>>,
    gate: Option<Arc<crate::policy::Gate>>,
    exec: bool,
    tools: Option<Arc<crate::executor::ToolTurn>>,
    unhealthy: bool,
    store: crate::store::Store,
    directory: std::path::PathBuf,
    runtime_id: String,
    ready: bool,
    agent: Arc<LoopService>,
    sessions: HashMap<String, Vec<Value>>,
    work: Work,
    spawner: Spawner,
    active: Option<(Command, TurnLease)>,
    done_tx: tokio::sync::mpsc::UnboundedSender<(String, Result<ModelResponse, String>)>,
    done_rx: tokio::sync::mpsc::UnboundedReceiver<(String, Result<ModelResponse, String>)>,
    outbox: Vec<Value>,
    model: String,
    event_rx: Option<tokio::sync::mpsc::UnboundedReceiver<ModelEvent>>,
}
impl Controller {
    pub fn new(
        agent: Arc<LoopService>,
        work: Work,
        spawner: Spawner,
        directory: std::path::PathBuf,
    ) -> DurableResult<Self> {
        let mut store = crate::store::Store::open(&directory).map_err(|e| e.to_string())?;
        let runtime_id = uuid::Uuid::new_v4().to_string();
        store.recover(&runtime_id).map_err(|e| e.to_string())?;
        let (done_tx, done_rx) = tokio::sync::mpsc::unbounded_channel();
        Ok(Self {
            policy: None,
            exec: false,
            tools: None,
            gate: None,
            unhealthy: false,
            store,
            directory,
            runtime_id,
            ready: false,
            agent,
            sessions: HashMap::new(),
            work,
            spawner,
            active: None,
            done_tx,
            done_rx,
            outbox: Vec::new(),
            model: "fixture".into(),
            event_rx: None,
        })
    }
    pub fn with_exec(mut self, enabled: bool) -> Self {
        self.exec = enabled;
        self
    }
    pub fn with_policy(mut self, policy: Arc<crate::policy::PolicyService>) -> Self {
        self.policy = Some(policy);
        self
    }
    pub fn with_model(mut self, model: String) -> Self {
        self.model = model;
        self
    }
    fn error(&self, code: &str) -> Value {
        json!({"version": VERSION, "runtime_id": self.runtime_id, "error": code})
    }
    async fn execute(&mut self, command: Command) -> DurableResult<Option<Value>> {
        let request = command.request_id().map(str::to_owned);
        let reply = self.dispatch(command).await?;
        Ok(reply.map(|mut reply| {
            if let Some(request) = request {
                reply["request_id"] = json!(request);
            }
            reply
        }))
    }
    async fn dispatch(&mut self, command: Command) -> DurableResult<Option<Value>> {
        if command.version() != VERSION {
            self.ready = false;
            return Ok(Some(self.error("incompatible_version")));
        }
        if let Command::Hello { capabilities, .. } = &command {
            if self.ready {
                return Ok(Some(self.error("already_initialized")));
            }
            let mut supported = CAPABILITIES.to_vec();
            if self.model == "fixture" {
                supported.push("fixture-model");
            }
            if !capabilities.iter().any(|c| c == "model-stream")
                || capabilities
                    .iter()
                    .any(|c| !supported.contains(&c.as_str()))
            {
                return Ok(Some(self.error("incompatible_capabilities")));
            }
            self.ready = true;
            return Ok(Some(
                json!({"version": VERSION, "runtime_id": self.runtime_id, "capabilities": supported, "model": self.model, "pid": std::process::id(), "sessions":self.store.sessions().map_err(|e|e.to_string())?}),
            ));
        }
        if !self.ready {
            return Ok(Some(self.error("handshake_required")));
        }
        let request_id = command.request_id().unwrap().to_owned();
        if request_id.trim().is_empty() {
            return Ok(Some(self.error("invalid_request_id")));
        }
        if matches!(command, Command::Turn { .. }) {
            if let Some((original, mut acceptance, result)) =
                self.store.receipt(&request_id).map_err(|e| e.to_string())?
            {
                if original != serde_json::to_value(&command).unwrap() {
                    return Ok(Some(self.error("request_id_conflict")));
                }
                acceptance["runtime_id"] = json!(self.runtime_id);
                acceptance["duplicate"] = json!(true);
                if let Some(mut result) = result {
                    self.outbox.push(acceptance);
                    result["runtime_id"] = json!(self.runtime_id);
                    result["duplicate"] = json!(true);
                    return Ok(Some(result));
                }
                return Ok(Some(acceptance));
            }
        }
        if !matches!(command, Command::Turn { .. }) {
            if let Some((original, mut reply, _)) =
                self.store.receipt(&request_id).map_err(|e| e.to_string())?
            {
                if original != serde_json::to_value(&command).unwrap() {
                    return Ok(Some(self.error("request_id_conflict")));
                }
                if let Command::Open {
                    session_id,
                    workspace,
                    ..
                } = &command
                {
                    self.store
                        .session(session_id, workspace.as_deref(), &self.directory)
                        .map_err(|e| e.to_string())?;
                    self.work.reopen_session(session_id);
                    self.sessions.insert(
                        session_id.clone(),
                        self.store.history(session_id).map_err(|e| e.to_string())?,
                    );
                }
                reply["runtime_id"] = json!(self.runtime_id);
                reply["duplicate"] = json!(true);
                return Ok(Some(reply));
            }
        }
        let mut reply = match &command {
            Command::Open {
                session_id,
                workspace,
                ..
            } => {
                if session_id.trim().is_empty() {
                    return Ok(Some(self.error("invalid_session_id")));
                }
                if let Err(e) =
                    self.store
                        .session(session_id, workspace.as_deref(), &self.directory)
                {
                    return Ok(Some(self.error(&e.to_string())));
                }
                self.work.reopen_session(session_id);
                self.sessions.insert(
                    session_id.clone(),
                    self.store.history(session_id).map_err(|e| e.to_string())?,
                );
                json!({"session_id": session_id, "status": "ready"})
            }
            Command::Replay {
                session_id, after, ..
            } => {
                let events = match self.store.events(session_id, *after) {
                    Ok(events) => events,
                    Err(error) => return Ok(Some(self.error(&error.to_string()))),
                };
                for mut event in events {
                    event["runtime_id"] = json!(self.runtime_id);
                    event["replay"] = json!(true);
                    self.outbox.push(event);
                }
                json!({"status":"replayed","session_id":session_id})
            }
            Command::History { session_id, .. } => {
                json!({"status":"history","session_id":session_id,"history":self.store.history(session_id).map_err(|e|e.to_string())?,"turns":self.store.completed(session_id).map_err(|e|e.to_string())?})
            }
            Command::Turn {
                session_id,
                turn_id,
                input,
                ..
            } => {
                if !self.sessions.contains_key(session_id) {
                    return Ok(Some(self.error("session_not_open")));
                }
                if turn_id.trim().is_empty()
                    || self.store.turn_exists(turn_id).map_err(|e| e.to_string())?
                {
                    return Ok(Some(self.error("turn_id_conflict")));
                }
                if self.unhealthy {
                    return Ok(Some(self.error("runtime_unhealthy")));
                }
                if self.active.is_some() {
                    return Ok(Some(self.error("busy")));
                }
                if self.store.blocked(session_id).map_err(|e| e.to_string())? {
                    return Ok(Some(self.error("session_outcome_unknown")));
                }
                let tools = if self.exec {
                    let Some(workspace) = self
                        .store
                        .workspace(session_id)
                        .map_err(|e| e.to_string())?
                    else {
                        return Ok(Some(self.error("workspace_required")));
                    };
                    Some(crate::executor::ToolTurn::new(
                        workspace.into(),
                        self.directory.clone(),
                        session_id.clone(),
                        turn_id.clone(),
                    ))
                } else {
                    None
                };
                let lease = match TurnLease::acquire() {
                    Ok(lease) => lease,
                    Err(error) => return Ok(Some(self.error(&error))),
                };
                let mut acceptance = json!({"version":VERSION,"runtime_id":self.runtime_id,"request_id":request_id,"session_id":session_id,"turn_id":turn_id,"status":"accepted"});
                self.store
                    .accept(&serde_json::to_value(&command).unwrap(), &mut acceptance)
                    .map_err(|e| e.to_string())?;
                self.outbox.push(acceptance);
                let agent = self.agent.clone();
                let mut model_input = self.sessions[session_id].clone();
                model_input.extend(ModelRequest::user(input).input);
                let input = ModelRequest { input: model_input };
                let (events, receiver) = tokio::sync::mpsc::unbounded_channel();
                self.event_rx = Some(receiver);
                let tx = self.done_tx.clone();
                let id = turn_id.clone();
                let owner = Owner::Turn {
                    session: session_id.clone(),
                    turn: turn_id.clone(),
                };
                let gate = self.policy.as_ref().map(|policy| policy.turn(turn_id));
                self.gate = gate.clone();
                self.tools = tools.clone();
                self.active = Some((command.clone(), lease));
                if let Err(error) = self.spawner.spawn(owner, async move {
                    let result = match gate {
                        Some(gate) => match tools {
                            Some(tools) => agent.0.run_with_tools(input, events, gate, tools).await,
                            None => agent.0.run_with_policy(input, events, gate).await,
                        },
                        None => agent.0.run(input, events).await,
                    };
                    let _ = tx.send((id, result));
                }) {
                    return Ok(Some(self.finish(Err(error)).await?));
                }
                eprintln!("Psi turn admitted: session={session_id} turn={turn_id}");
                return Ok(None);
            }

            Command::Cancel {
                session_id,
                turn_id,
                ..
            } => {
                if self.active.as_ref().is_some_and(|(c, _)| matches!(c, Command::Turn { session_id: active, turn_id: active_turn, .. } if active == session_id && turn_id.as_ref().is_none_or(|id| id == active_turn))) {
                    let cancelled = self.finish(Err("local_cancelled".into())).await?;
                    self.outbox.push(cancelled);
                }
                json!({"status":"cancel_requested", "session_id":session_id})
            }
            Command::Close { session_id, .. } => {
                if self.active.as_ref().is_some_and(|(c, _)| matches!(c, Command::Turn { session_id: active, .. } if active == session_id)) {
                    let cancelled = self.finish(Err("session_closed".into())).await?;
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
                    let cancelled = self.finish(Err("runtime_closed".into())).await?;
                    self.outbox.push(cancelled);
                }
                json!({"status": "shutdown"})
            }
            Command::Hello { .. } => unreachable!(),
        };
        reply["version"] = json!(VERSION);
        reply["runtime_id"] = json!(self.runtime_id);
        reply["request_id"] = json!(request_id);
        self.store
            .accept(&serde_json::to_value(&command).unwrap(), &mut reply)
            .map_err(|e| e.to_string())?;
        Ok(Some(reply))
    }
    async fn finish(&mut self, mut result: Result<ModelResponse, String>) -> DurableResult<Value> {
        let (command, _lease) = self.active.take().unwrap();
        self.event_rx = None;
        if let Some(gate) = &self.gate {
            gate.close();
        }
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
        if let Some(tools) = self.tools.take() {
            match tools.cleanup().await {
                Ok(Some(value)) => reply["tool_result"] = value,
                Ok(None) => {}
                Err(error) => {
                    self.unhealthy = true;
                    reply["cleanup_error"] = json!(error);
                    result = Err(error);
                }
            }
        }
        if let Some(gate) = self.gate.take() {
            let event = if result.is_ok() {
                crate::policy::Event::Finish
            } else {
                crate::policy::Event::Cancel
            };
            if let Err(error) = gate.cleanup(event).await {
                self.unhealthy = true;
                reply["cleanup_error"] = json!(error);
                result = Err(error);
            }
            reply["policy"] = gate.evidence();
        }
        match result {
            Ok(output) => {
                reply["status"] = json!("completed");
                reply["output"] = json!(output.text);
                reply["provider_output"] = json!(output.output);
                reply["response_id"] = json!(output.response_id);
                reply["input"] = json!(input);
                let history = self.sessions.get_mut(session_id).unwrap();
                history.extend(ModelRequest::user(input).input);
                history.extend(output.output);
            }
            Err(reason) => {
                reply["status"] = json!(if matches!(
                    reason.as_str(),
                    "session_closed" | "runtime_closed" | "controller_closed" | "local_cancelled"
                ) {
                    "cancelled"
                } else {
                    "failed"
                });
                reply["error"] = json!(reason);
                if reply["status"] == "cancelled" {
                    reply["cancellation_scope"] = json!(if reply.get("tool_result").is_some() {
                        "local_tools"
                    } else if reply.get("policy").is_some() {
                        "local_policy"
                    } else {
                        "local_only_server_processing_and_billing_unknown"
                    });
                }
            }
        }
        self.store
            .finish(&mut reply, Some(&self.sessions[session_id]))
            .map_err(|e| e.to_string())?;
        eprintln!("Psi turn cleaned: tasks={}", self.work.count());
        Ok(reply)
    }
    pub async fn serve(&mut self) -> Result<(), String> {
        let result = self.serve_inner().await;
        if self.active.is_some() {
            self.finish(Err("controller_closed".into())).await?;
        }
        result
    }
    async fn serve_inner(&mut self) -> Result<(), String> {
        let mut input = BufReader::new(tokio::io::stdin());
        let mut output = tokio::io::stdout();
        let mut line = Vec::new();
        loop {
            let reply = tokio::select! {
                biased;
                frame = read_frame(&mut input, &mut line) => {
                    if !frame? { break; }
                    let parsed = serde_json::from_slice::<Command>(&line);
                    line.clear();
                    match parsed {
                        Ok(command) => self.execute(command).await?,
                        Err(_) => Some(self.error("invalid_command")),
                    }
                }
                Some(event) = async { match &mut self.event_rx { Some(rx) => rx.recv().await, None => std::future::pending().await } } => {
                    let mut frame = self.active.as_ref().map(|(c, _)| {
                        let Command::Turn { request_id, session_id, turn_id, .. } = c else { unreachable!() };
                        json!({"version":VERSION,"runtime_id":self.runtime_id,"request_id":request_id,"session_id":session_id,"turn_id":turn_id,"status":"streaming","event":event})
                    });
                    if let Some(frame) = &mut frame { self.store.event(frame).map_err(|e|e.to_string())?; }
                    frame
                }
                Some((id, result)) = self.done_rx.recv() => {
                    if self.active.as_ref().is_some_and(|(c, _)| matches!(c, Command::Turn { turn_id, .. } if turn_id == &id)) {
                        Some(self.finish(result).await?)
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
