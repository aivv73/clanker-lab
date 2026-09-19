//! The model and orchestration are replaceable services, outside the lifecycle core.
use crate::core::{Context, Plugin};
use async_trait::async_trait;
use std::sync::Arc;

#[async_trait]
pub trait Model: Send + Sync {
    async fn respond(&self, input: &str) -> Result<String, String>;
}
pub struct ModelService(pub Arc<dyn Model>);
struct FixtureModel;
#[async_trait]
impl Model for FixtureModel {
    async fn respond(&self, input: &str) -> Result<String, String> {
        Ok(format!("Psi fixture: {input}"))
    }
}
pub struct FixtureModelPlugin;
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
        context.register("model", ModelService(Arc::new(FixtureModel)))
    }
    async fn close(&mut self) {}
}

#[async_trait]
pub trait AgentLoop: Send + Sync {
    async fn run(&self, input: &str) -> Result<String, String>;
}
pub struct LoopService(pub Arc<dyn AgentLoop>);
struct SingleResponseLoop {
    model: Arc<ModelService>,
}
#[async_trait]
impl AgentLoop for SingleResponseLoop {
    async fn run(&self, input: &str) -> Result<String, String> {
        self.model.0.respond(input).await
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
    async fn close(&mut self) {}
}
