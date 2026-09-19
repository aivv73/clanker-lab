//! The model and orchestration are replaceable services, outside the lifecycle core.
use crate::core::{Context, Plugin};
use async_trait::async_trait;
use std::sync::Arc;

#[async_trait]
pub trait Model: Send + Sync {
    async fn respond(&self, input: &str) -> Result<String, String>;
}
pub struct ModelService(pub Arc<dyn Model>);
struct FixtureModel {
    gate: Option<std::path::PathBuf>,
}
#[async_trait]
impl Model for FixtureModel {
    async fn respond(&self, input: &str) -> Result<String, String> {
        if let Some(gate) = &self.gate {
            while !gate.exists() {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        }
        Ok(format!("Psi fixture: {input}"))
    }
}
pub struct FixtureModelPlugin {
    pub gate: Option<std::path::PathBuf>,
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
            ModelService(Arc::new(FixtureModel {
                gate: self.gate.clone(),
            })),
        )
    }
    async fn close(&mut self) -> Result<(), String> {
        Ok(())
    }
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
    async fn run(&self, input: &str) -> Result<String, String> {
        let mut responses = Vec::new();
        responses.push(self.model.0.respond(input).await?);
        Ok(responses.join(""))
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
