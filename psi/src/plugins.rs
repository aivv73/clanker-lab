//! The model and orchestration are replaceable services, outside the lifecycle core.
use crate::core::{Context, Plugin};
use crate::model::{Events, ModelEvent, ModelRequest, ModelResponse};
use async_trait::async_trait;
use std::sync::Arc;

#[async_trait]
pub trait Model: Send + Sync {
    async fn respond(&self, request: ModelRequest, events: Events)
    -> Result<ModelResponse, String>;
}
pub struct ModelService(pub Arc<dyn Model>);
struct FixtureModel {
    gate: Option<std::path::PathBuf>,
}
#[async_trait]
impl Model for FixtureModel {
    async fn respond(
        &self,
        request: ModelRequest,
        events: Events,
    ) -> Result<ModelResponse, String> {
        if let Some(gate) = &self.gate {
            while !gate.exists() {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        }
        let input = request
            .input
            .last()
            .and_then(|v| v["content"].as_str())
            .unwrap_or("");
        let text = format!("Psi fixture: {input}");
        let _ = events.send(ModelEvent::Text {
            delta: text.clone(),
        });
        Ok(ModelResponse {
            output: vec![
                serde_json::json!({"type":"message","role":"assistant","content":[{"type":"output_text","text":text}]}),
            ],
            text,
            response_id: None,
        })
    }
}
pub struct FixtureModelPlugin {
    pub gate: Option<std::path::PathBuf>,
    pub exec: bool,
}
#[async_trait]
impl Plugin for FixtureModelPlugin {
    fn id(&self) -> &'static str {
        "fixture-model"
    }
    fn provides(&self) -> &'static [&'static str] {
        &["model"]
    }
    fn requires(&self) -> &'static [&'static str] {
        &[]
    }
    async fn start(&mut self, mut context: Context<'_>) -> Result<(), String> {
        context.register(
            "model",
            ModelService(if self.exec {
                Arc::new(ExecFixtureModel)
            } else {
                Arc::new(FixtureModel {
                    gate: self.gate.clone(),
                })
            }),
        )
    }
    async fn close(&mut self) -> Result<(), String> {
        Ok(())
    }
}

#[async_trait]
pub trait AgentLoop: Send + Sync {
    async fn run(&self, input: ModelRequest, events: Events) -> Result<ModelResponse, String>;
    async fn run_with_tools(
        &self,
        input: ModelRequest,
        events: Events,
        policy: Arc<crate::policy::Gate>,
        _tools: Arc<crate::executor::ToolTurn>,
    ) -> Result<ModelResponse, String> {
        self.run_with_policy(input, events, policy).await
    }
    async fn run_with_policy(
        &self,
        input: ModelRequest,
        events: Events,
        _policy: Arc<crate::policy::Gate>,
    ) -> Result<ModelResponse, String> {
        self.run(input, events).await
    }
}
pub struct LoopService(pub Arc<dyn AgentLoop>);
struct SingleResponseLoop {
    model: Arc<ModelService>,
}
#[async_trait]
impl AgentLoop for SingleResponseLoop {
    async fn run(&self, input: ModelRequest, events: Events) -> Result<ModelResponse, String> {
        self.model.0.respond(input, events).await
    }
}
pub struct SingleResponsePlugin;
#[async_trait]
impl Plugin for SingleResponsePlugin {
    fn id(&self) -> &'static str {
        "single-response-loop"
    }
    fn provides(&self) -> &'static [&'static str] {
        &["agent-loop"]
    }
    fn requires(&self) -> &'static [&'static str] {
        &["model"]
    }
    async fn start(&mut self, mut context: Context<'_>) -> Result<(), String> {
        let model = context.dependency("model")?;
        context.register(
            "agent-loop",
            LoopService(Arc::new(SingleResponseLoop { model })),
        )
    }
    async fn close(&mut self) -> Result<(), String> {
        Ok(())
    }
}

// A distinct implementation of the same loop contract. Selection belongs to assembly.
struct BufferedResponseLoop {
    model: Arc<ModelService>,
}
#[async_trait]
impl AgentLoop for BufferedResponseLoop {
    async fn run(&self, input: ModelRequest, events: Events) -> Result<ModelResponse, String> {
        let (buffer, mut receiver) = tokio::sync::mpsc::unbounded_channel();
        let forward = async {
            let mut buffered = Vec::new();
            while let Some(event) = receiver.recv().await {
                if matches!(event, ModelEvent::SignIn { .. }) {
                    let _ = events.send(event);
                } else {
                    buffered.push(event);
                }
            }
            for event in buffered {
                let _ = events.send(event);
            }
        };
        let (result, ()) = tokio::join!(self.model.0.respond(input, buffer), forward);
        result
    }
}
pub struct BufferedResponsePlugin;
#[async_trait]
impl Plugin for BufferedResponsePlugin {
    fn id(&self) -> &'static str {
        "buffered-response-loop"
    }
    fn provides(&self) -> &'static [&'static str] {
        &["agent-loop"]
    }
    fn requires(&self) -> &'static [&'static str] {
        &["model"]
    }
    async fn start(&mut self, mut context: Context<'_>) -> Result<(), String> {
        let model = context.dependency("model")?;
        context.register(
            "agent-loop",
            LoopService(Arc::new(BufferedResponseLoop { model })),
        )
    }
    async fn close(&mut self) -> Result<(), String> {
        Ok(())
    }
}

/// Explicit fixture startup faults, never enabled by prompt content.
pub struct StartupProbe(pub &'static str);
#[async_trait]
impl Plugin for StartupProbe {
    fn id(&self) -> &'static str {
        "startup-probe"
    }
    fn provides(&self) -> &'static [&'static str] {
        match self.0 {
            "duplicate" => &["model"],
            "cycle" => &["cycle"],
            _ => &["probe"],
        }
    }
    fn requires(&self) -> &'static [&'static str] {
        match self.0 {
            "missing" => &["absent"],
            "cycle" => &["cycle"],
            _ => &["agent-loop"],
        }
    }
    async fn start(&mut self, mut context: Context<'_>) -> Result<(), String> {
        context.register("probe", 1usize)?;
        struct Resource;
        impl Drop for Resource {
            fn drop(&mut self) {
                eprintln!("Psi probe resource dropped");
            }
        }
        let resource = Resource;
        context
            .spawner()
            .spawn(crate::work::Owner::Runtime, async move {
                let _resource = resource;
                std::future::pending::<()>().await;
            })?;
        Err("injected startup failure".into())
    }
    async fn close(&mut self) -> Result<(), String> {
        eprintln!("Psi probe closed");
        Ok(())
    }
}

/// Deterministic vertical slice: one harmless action, admitted by the real policy.
struct PolicyFixtureLoop;
#[async_trait]
impl AgentLoop for PolicyFixtureLoop {
    async fn run(&self, _: ModelRequest, _: Events) -> Result<ModelResponse, String> {
        Err("policy_context_required".into())
    }
    async fn run_with_policy(
        &self,
        _: ModelRequest,
        events: Events,
        policy: Arc<crate::policy::Gate>,
    ) -> Result<ModelResponse, String> {
        let (count, prefix) = policy.fixture(&events).await?;
        let text = format!("Psi policy fixture: counter={count}; admitted by Bend.");
        let _ = events.send(ModelEvent::Text {
            delta: text.clone(),
        });
        let text = format!("{prefix}{text}");
        Ok(ModelResponse {
            output: vec![
                serde_json::json!({"type":"message","role":"assistant","content":[{"type":"output_text","text":text}]}),
            ],
            text,
            response_id: None,
        })
    }
}
pub struct PolicyFixturePlugin;
#[async_trait]
impl Plugin for PolicyFixturePlugin {
    fn id(&self) -> &'static str {
        "policy-fixture-loop"
    }
    fn provides(&self) -> &'static [&'static str] {
        &["agent-loop"]
    }
    fn requires(&self) -> &'static [&'static str] {
        &["policy"]
    }
    async fn start(&mut self, mut context: Context<'_>) -> Result<(), String> {
        let _: Arc<crate::policy::PolicyService> = context.dependency("policy")?;
        context.register("agent-loop", LoopService(Arc::new(PolicyFixtureLoop)))
    }
    async fn close(&mut self) -> Result<(), String> {
        Ok(())
    }
}

/// Deterministic model emits one function call with the prompt's JSON arguments.
struct ExecFixtureModel;
#[async_trait]
impl Model for ExecFixtureModel {
    async fn respond(&self, request: ModelRequest, _: Events) -> Result<ModelResponse, String> {
        let arguments = request
            .input
            .last()
            .and_then(|i| i["content"].as_str())
            .ok_or("invalid_fixture_command")?;
        let _: serde_json::Value =
            serde_json::from_str(arguments).map_err(|_| "invalid_fixture_command")?;
        Ok(ModelResponse {
            text: String::new(),
            output: vec![
                serde_json::json!({"type":"function_call","name":"exec_command","call_id":format!("exec-{}",uuid::Uuid::new_v4()),"arguments":arguments}),
            ],
            response_id: None,
        })
    }
}
struct ExecFixtureLoop {
    model: Arc<ModelService>,
}
#[async_trait]
impl AgentLoop for ExecFixtureLoop {
    async fn run(&self, _: ModelRequest, _: Events) -> Result<ModelResponse, String> {
        Err("tool_context_required".into())
    }
    async fn run_with_tools(
        &self,
        input: ModelRequest,
        events: Events,
        policy: Arc<crate::policy::Gate>,
        tools: Arc<crate::executor::ToolTurn>,
    ) -> Result<ModelResponse, String> {
        let response = self.model.0.respond(input, events.clone()).await?;
        let call = response
            .output
            .into_iter()
            .next()
            .ok_or("missing_tool_call")?;
        let value: serde_json::Value =
            serde_json::from_str(call["arguments"].as_str().ok_or("invalid_tool_call")?)
                .map_err(|_| "invalid_fixture_command")?;
        let mut commands: std::collections::VecDeque<serde_json::Value> =
            if let Some(commands) = value.get("commands") {
                commands
                    .as_array()
                    .ok_or("invalid_fixture_command")?
                    .clone()
                    .into()
            } else {
                vec![serde_json::json!({"name":"exec_command","arguments":value})].into()
            };
        let mut process = None;
        let mut output = Vec::new();
        let mut text = String::new();
        while let Some(command) = commands.pop_front().or_else(|| {
            process
                .as_ref()
                .map(|id| serde_json::json!({"name":"write_stdin","arguments":{"session_id":id}}))
        }) {
            let name = command["name"].as_str().ok_or("invalid_tool_call")?;
            let mut arguments = command["arguments"].clone();
            if arguments["session_id"] == "$process" {
                arguments["session_id"] =
                    serde_json::json!(process.as_ref().ok_or("no_running_process")?);
            }
            let call_id = format!("tool-{}", uuid::Uuid::new_v4());
            let call = serde_json::json!({"type":"function_call","name":name,"call_id":call_id,"arguments":arguments.to_string()});
            let _ = events.send(ModelEvent::OutputItem {
                index: output.len() as u64,
                item: call.clone(),
            });
            output.push(call);
            let result = match tools
                .invoke_identified(name, arguments, policy.clone(), call_id.clone())
                .await
            {
                Ok(value) => value,
                Err(error)
                    if process.is_some()
                        && matches!(
                            error.as_str(),
                            "tool_busy"
                                | "unknown_process"
                                | "unsupported_tool"
                                | "unsupported_exec_arguments"
                                | "unsupported_write_arguments"
                                | "unsupported_workdir"
                        ) =>
                {
                    serde_json::json!({"reason":"rejected","error":error})
                }
                Err(error) => return Err(error),
            };
            if result["reason"] != "running" && result["cleanup_confirmed"] == false {
                return Err("executor_cleanup_unconfirmed".into());
            }
            if result["reason"] == "running" {
                process = result["session_id"].as_str().map(str::to_owned);
            } else if result["cleanup_confirmed"] == true {
                process = None;
            }
            let item = serde_json::json!({"type":"function_call_output","call_id":call_id,"output":result.to_string()});
            let _ = events.send(ModelEvent::OutputItem {
                index: output.len() as u64,
                item: item.clone(),
            });
            output.push(item);
            let delta = format!(
                "{}: {}\n{}{}",
                name,
                result["error"]
                    .as_str()
                    .or(result["reason"].as_str())
                    .unwrap_or("unknown"),
                result["output"].as_str().unwrap_or(""),
                if result["truncated"] == true {
                    "\n[output truncated]"
                } else {
                    ""
                }
            );
            let _ = events.send(ModelEvent::Text {
                delta: delta.clone(),
            });
            text.push_str(&delta);
        }
        output.push(serde_json::json!({"type":"message","role":"assistant","content":[{"type":"output_text","text":text}]}));
        Ok(ModelResponse {
            text,
            output,
            response_id: None,
        })
    }
}
pub struct ExecFixturePlugin;
#[async_trait]
impl Plugin for ExecFixturePlugin {
    fn id(&self) -> &'static str {
        "exec-fixture-loop"
    }
    fn provides(&self) -> &'static [&'static str] {
        &["agent-loop"]
    }
    fn requires(&self) -> &'static [&'static str] {
        &["policy", "model"]
    }
    async fn start(&mut self, mut context: Context<'_>) -> Result<(), String> {
        let _: Arc<crate::policy::PolicyService> = context.dependency("policy")?;
        context.register(
            "agent-loop",
            LoopService(Arc::new(ExecFixtureLoop {
                model: context.dependency("model")?,
            })),
        )
    }
    async fn close(&mut self) -> Result<(), String> {
        Ok(())
    }
}
