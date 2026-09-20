//! Account-free tests at the actual model service boundary with a local OAuth/SSE peer.
use psi::{
    codex::{CodexModel, Config},
    model::{ModelEvent, ModelRequest, ModelResponse},
    plugins::Model,
};
use serde_json::Value;
use std::{
    io::{BufRead, BufReader},
    process::{Child, Command, Stdio},
    time::Duration,
};

struct Fixture {
    child: Child,
    base: String,
}
impl Fixture {
    fn new(scenario: &str) -> Self {
        let mut child = Command::new("python3")
            .arg(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/fixtures/oauth_server.py"
            ))
            .arg(scenario)
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let mut base = String::new();
        BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut base)
            .unwrap();
        assert!(base.starts_with("http://127.0.0.1:"));
        Self {
            child,
            base: base.trim().into(),
        }
    }
    fn model(&self) -> CodexModel {
        CodexModel::new(Config::fixture(&self.base, "fixture-codex".into()).unwrap()).unwrap()
    }
    async fn evidence(&self) -> Value {
        reqwest::Client::builder()
            .no_proxy()
            .build()
            .unwrap()
            .get(format!("{}/evidence", self.base))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
async fn answer(
    model: &CodexModel,
    request: ModelRequest,
) -> (Result<ModelResponse, String>, Vec<ModelEvent>) {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let collect = async {
        let mut events = Vec::new();
        while let Some(event) = rx.recv().await {
            if let ModelEvent::SignIn { url } = &event {
                reqwest::Client::builder()
                    .no_proxy()
                    .build()
                    .unwrap()
                    .get(url)
                    .send()
                    .await
                    .unwrap();
            }
            events.push(event);
        }
        events
    };
    tokio::time::timeout(Duration::from_secs(15), async {
        tokio::join!(model.respond(request, tx), collect)
    })
    .await
    .unwrap()
}

#[tokio::test]
async fn pkce_rotation_lossless_items_and_exact_followup_input() {
    let fixture = Fixture::new("rotation");
    let model = fixture.model();
    let (first, events) = answer(&model, ModelRequest::user("hello")).await;
    let first = first.unwrap();
    assert_eq!(first.text, "Psi HTTP fixture: Привет 🌍");
    assert_eq!(first.output[0]["encrypted_content"], "opaque-reasoning");
    assert_eq!(first.output[2]["id"], "fc_47");
    assert_eq!(first.output[2]["call_id"], "call_47");
    assert_eq!(first.output[2]["arguments"], "{\"path\":\"target.txt\"}");
    assert!(
        events
            .iter()
            .any(|e| matches!(e, ModelEvent::Reasoning { .. }))
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, ModelEvent::Text { delta } if delta == &first.text))
    );
    let mut input = ModelRequest {
        input: first.output,
    };
    input.input.push(serde_json::json!({"type":"function_call_output","call_id":"call_47","output":"synthetic result"}));
    let (second, events) = answer(&model, input.clone()).await;
    second.unwrap();
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, ModelEvent::SignIn { .. }))
    );
    let evidence = fixture.evidence().await;
    assert_eq!(evidence["authorizations"], 1);
    assert_eq!(evidence["exchanges"], 1);
    assert_eq!(evidence["rotations"], 2);
    assert_eq!(
        evidence["requests"][1]["input"],
        serde_json::json!(input.input)
    );
    assert!(
        !serde_json::to_string(&evidence["requests"])
            .unwrap()
            .contains("synthetic-refresh")
    );
    drop(model);
    let (_, events) = answer(&fixture.model(), ModelRequest::user("new runtime")).await;
    assert!(
        events
            .iter()
            .any(|e| matches!(e, ModelEvent::SignIn { .. }))
    );
    assert_eq!(fixture.evidence().await["authorizations"], 2);
}

#[tokio::test]
async fn stream_and_auth_failures_are_explicit_and_do_not_leak_bodies() {
    for (scenario, expected) in [
        ("state", "sign_in_state_mismatch"),
        ("duplicate-state", "sign_in_state_mismatch"),
        ("denied", "sign_in_denied"),
        ("sign-in-failed", "sign_in_failed"),
        ("refresh-failed", "refresh_failed"),
        ("401", "model_unauthorized"),
        ("mime", "model_mime_json"),
        ("html-mime", "model_mime_html"),
        ("missing-mime-unfinished", "model_unfinished_stream"),
        ("missing-mime-json", "model_unfinished_stream"),
        ("missing-mime-html", "model_unfinished_stream"),
        ("unfinished", "model_unfinished_stream"),
        ("done-only", "model_unfinished_stream"),
        ("broken", "model_transport"),
        ("incomplete", "model_incomplete"),
        ("failed", "model_failed"),
        ("invalid-utf8", "model_invalid_utf8"),
        ("invalid-json", "model_invalid_json"),
        ("mismatch", "model_item_mismatch"),
        ("limit", "model_stream_limit"),
    ] {
        let fixture = Fixture::new(scenario);
        let model = fixture.model();
        let (result, events) = answer(&model, ModelRequest::user("hello")).await;
        assert_eq!(result.unwrap_err(), expected, "{scenario}");
        if scenario == "401" {
            let (retry, _) = answer(&model, ModelRequest::user("explicit retry")).await;
            assert_eq!(retry.unwrap_err(), "model_unauthorized");
            let evidence = fixture.evidence().await;
            assert_eq!(evidence["authorizations"], 2);
            assert_eq!(evidence["requests"].as_array().unwrap().len(), 2);
        }
        if scenario == "incomplete" {
            assert!(events.iter().any(|event| matches!(event, ModelEvent::OutputItem { item, .. } if item["encrypted_content"] == "opaque-reasoning")));
        }
        if scenario.contains("state") {
            assert_eq!(fixture.evidence().await["exchanges"], 0);
        }
    }
}

#[tokio::test]
async fn sse_line_endings_multiline_and_output_item_fallback() {
    for scenario in ["normal", "cr", "multiline", "fallback", "missing-mime"] {
        let fixture = Fixture::new(scenario);
        let (result, _) = answer(&fixture.model(), ModelRequest::user("hello")).await;
        let response = result.unwrap();
        assert_eq!(response.text, "Psi HTTP fixture: Привет 🌍", "{scenario}");
        assert_eq!(response.output[0]["id"], "rs_47");
    }
}

#[tokio::test]
async fn dropping_request_closes_local_stream_without_claiming_remote_cancellation() {
    let fixture = Fixture::new("hold");
    let model = fixture.model();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let mut response = Box::pin(model.respond(ModelRequest::user("hello"), tx));
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            tokio::select! {
                result = &mut response => panic!("unfinished response must not complete: {result:?}"),
                Some(event) = rx.recv() => match event {
                    ModelEvent::SignIn { url } => { // Drive callback concurrently with model future.
                        let browser = reqwest::Client::builder().no_proxy().build().unwrap().get(url).send();
                        tokio::select! { result = &mut response => panic!("{result:?}"), _ = browser => {} }
                    }
                    ModelEvent::Text { .. } => break,
                    _ => {}
                }
            }
        }
    }).await.unwrap();
    drop(response);
    tokio::time::timeout(Duration::from_secs(5), async {
        while fixture.evidence().await["disconnected"] != true {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
}
