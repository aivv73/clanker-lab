//! Provider-neutral request/response boundary. Provider items remain lossless JSON.
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::mpsc::UnboundedSender;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ModelRequest {
    pub input: Vec<Value>,
}
impl ModelRequest {
    pub fn user(text: &str) -> Self {
        Self {
            input: vec![json!({"role":"user","content":text})],
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ModelResponse {
    pub text: String,
    pub output: Vec<Value>,
    pub response_id: Option<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ModelEvent {
    Text { delta: String },
    Reasoning { delta: String },
    SignIn { url: String },
    OutputItem { index: u64, item: Value },
}
pub type Events = UnboundedSender<ModelEvent>;
