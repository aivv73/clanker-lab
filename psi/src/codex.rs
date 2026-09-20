//! Independent ChatGPT OAuth + Responses client. No credential files or Codex IPC.
use crate::{
    core::{Context, Plugin},
    model::{Events, ModelEvent, ModelRequest, ModelResponse},
    plugins::{Model, ModelService},
};
use async_trait::async_trait;
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use futures_util::StreamExt;
use reqwest::{Client, Url};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    io::Read,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    sync::Mutex,
};

const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
type Result<T> = std::result::Result<T, String>;

#[derive(Clone)]
pub struct Config {
    authorize: String,
    token: String,
    responses: String,
    callback_port: u16,
    local_fixture: bool,
    pub model: String,
}
impl Config {
    pub fn live(model: String) -> Self {
        Self {
            authorize: "https://auth.openai.com/oauth/authorize".into(),
            token: "https://auth.openai.com/oauth/token".into(),
            responses: "https://chatgpt.com/backend-api/codex/responses".into(),
            callback_port: 1455,
            local_fixture: false,
            model,
        }
    }
    /// Explicit local fixture mode; never send live credentials to configurable hosts.
    pub fn fixture(base: &str, model: String) -> Result<Self> {
        let u = Url::parse(base).map_err(|_| "invalid_fixture_url")?;
        if u.scheme() != "http"
            || u.host_str() != Some("127.0.0.1")
            || !u.username().is_empty()
            || u.password().is_some()
            || u.query().is_some()
            || u.fragment().is_some()
            || u.path() != "/"
        {
            return Err("fixture_requires_loopback_origin".into());
        }
        let base = base.trim_end_matches('/');
        Ok(Self {
            authorize: format!("{base}/authorize"),
            token: format!("{base}/token"),
            responses: format!("{base}/responses"),
            callback_port: 0,
            local_fixture: true,
            model,
        })
    }
}
// Deliberately no Debug/Serialize: tokens never cross the model/protocol boundary.
struct Tokens {
    access: String,
    refresh: String,
    account: String,
    expires: Instant,
}
pub struct CodexModel {
    config: Config,
    client: Client,
    tokens: Mutex<Option<Tokens>>,
}
impl CodexModel {
    pub fn new(config: Config) -> Result<Self> {
        let builder = Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(15));
        // Live auth/model traffic must use the user's network configuration.
        // Only synthetic loopback fixtures bypass inherited proxies.
        let builder = if config.local_fixture {
            builder.no_proxy()
        } else {
            builder
        };
        let client = builder.build().map_err(|_| "client_init_failed")?;
        Ok(Self {
            config,
            client,
            tokens: Mutex::new(None),
        })
    }
    async fn sign_in(&self, events: &Events) -> Result<Tokens> {
        let listener =
            TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, self.config.callback_port))
                .await
                .map_err(|_| "sign_in_callback_unavailable")?;
        let redirect = format!(
            "http://localhost:{}/auth/callback",
            listener
                .local_addr()
                .map_err(|_| "sign_in_callback_unavailable")?
                .port()
        );
        let verifier = random()?;
        let state = random()?;
        let mut url = Url::parse(&self.config.authorize).map_err(|_| "sign_in_url")?;
        url.query_pairs_mut().extend_pairs([
            ("response_type", "code"),
            ("client_id", CLIENT_ID),
            ("redirect_uri", &redirect),
            ("scope", "openid profile email offline_access"),
            ("code_challenge_method", "S256"),
            (
                "code_challenge",
                &URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes())),
            ),
            ("state", &state),
            ("id_token_add_organizations", "true"),
            ("codex_cli_simplified_flow", "true"),
            ("originator", "psi"),
        ]);
        events
            .send(ModelEvent::SignIn { url: url.into() })
            .map_err(|_| "local_cancelled")?;
        let code = tokio::time::timeout(Duration::from_secs(300), callback(listener, &state))
            .await
            .map_err(|_| "sign_in_timeout")??;
        self.exchange(
            &[
                ("grant_type", "authorization_code"),
                ("client_id", CLIENT_ID),
                ("redirect_uri", &redirect),
                ("code", &code),
                ("code_verifier", &verifier),
            ],
            "sign_in",
        )
        .await
    }
    async fn exchange(&self, form: &[(&str, &str)], stage: &str) -> Result<Tokens> {
        // Never report response bodies, request URLs, headers, or reqwest errors.
        let response = self
            .client
            .post(&self.config.token)
            .form(form)
            .timeout(Duration::from_secs(30))
            .send()
            .await
            .map_err(|_| format!("{stage}_transport"))?;
        if !response.status().is_success() {
            return Err(format!("{stage}_failed"));
        }
        let value = limited_json(response)
            .await
            .map_err(|_| format!("{stage}_invalid_response"))?;
        parse_tokens(value).map_err(|_| format!("{stage}_invalid_response"))
    }
    async fn request(
        &self,
        tokens: &Tokens,
        request: ModelRequest,
        events: Events,
    ) -> Result<ModelResponse> {
        let body = json!({"model":self.config.model,"store":false,"stream":true,"instructions":"You are Psi, a helpful assistant. No tools are available in this slice.","input":request.input,"include":["reasoning.encrypted_content"],"parallel_tool_calls":false,"tools":[]});
        let response = self
            .client
            .post(&self.config.responses)
            .bearer_auth(&tokens.access)
            .header("chatgpt-account-id", &tokens.account)
            .header("originator", "psi")
            .header("User-Agent", "psi/0.1.0")
            .header("OpenAI-Beta", "responses=experimental")
            .header("accept", "text/event-stream")
            .json(&body)
            .timeout(Duration::from_secs(300))
            .send()
            .await
            .map_err(|_| "model_transport")?;
        if response.status().as_u16() == 401 {
            return Err("model_unauthorized".into());
        }
        if !response.status().is_success() {
            return Err("model_http_failed".into());
        }
        // The pinned Codex backend can omit Content-Type on a valid SSE response.
        // Missing metadata still requires valid framing and explicit completion below.
        // An explicitly incompatible type remains an error; never parse HTML as success.
        if let Some(header) = response.headers().get("content-type") {
            let mime = header
                .to_str()
                .unwrap_or("")
                .split(';')
                .next()
                .unwrap_or("")
                .trim();
            if !mime.eq_ignore_ascii_case("text/event-stream") {
                let kind = if mime.eq_ignore_ascii_case("application/json") {
                    "json"
                } else if mime.eq_ignore_ascii_case("text/html") {
                    "html"
                } else {
                    "other"
                };
                return Err(format!("model_mime_{kind}"));
            }
        }
        let mut stream = response.bytes_stream();
        let mut sse = Sse::default();
        let mut items = BTreeMap::<u64, Value>::new();
        let mut streamed_text = String::new();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|_| "model_transport")?;
            for event in sse.chunk(&chunk)? {
                match event["type"].as_str().unwrap_or("") {
                    "response.output_text.delta" | "response.refusal.delta" => {
                        let delta = event["delta"]
                            .as_str()
                            .ok_or("model_invalid_event")?
                            .to_owned();
                        streamed_text.push_str(&delta);
                        events
                            .send(ModelEvent::Text { delta })
                            .map_err(|_| "local_cancelled")?;
                    }
                    "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
                        let delta = event["delta"]
                            .as_str()
                            .ok_or("model_invalid_event")?
                            .to_owned();
                        events
                            .send(ModelEvent::Reasoning { delta })
                            .map_err(|_| "local_cancelled")?;
                    }
                    "response.output_item.done" => {
                        let index = event["output_index"].as_u64().ok_or("model_invalid_item")?;
                        if !event["item"].is_object() || items.contains_key(&index) {
                            return Err("model_invalid_item".into());
                        }
                        let item = event["item"].clone();
                        events
                            .send(ModelEvent::OutputItem {
                                index,
                                item: item.clone(),
                            })
                            .map_err(|_| "local_cancelled")?;
                        items.insert(index, item);
                    }
                    "response.incomplete" => return Err("model_incomplete".into()),
                    "response.failed" | "error" => return Err("model_failed".into()),
                    "response.completed" | "response.done" => {
                        let response = &event["response"];
                        if response["status"] != "completed" {
                            return Err("model_incomplete".into());
                        }
                        let mut output = response["output"]
                            .as_array()
                            .ok_or("model_invalid_completion")?
                            .clone();
                        if output.is_empty() && !items.is_empty() {
                            if items.keys().copied().ne(0..items.len() as u64) {
                                return Err("model_invalid_item".into());
                            }
                            output = items.into_values().collect();
                        } else {
                            for (index, item) in &items {
                                if output.get(*index as usize) != Some(item) {
                                    return Err("model_item_mismatch".into());
                                }
                            }
                        }
                        let mut text = String::new();
                        for item in &output {
                            if !item.is_object() {
                                return Err("model_invalid_item".into());
                            }
                            if item["type"] == "function_call"
                                && (item["call_id"].as_str().is_none()
                                    || item["name"].as_str().is_none()
                                    || item["arguments"].as_str().is_none())
                            {
                                return Err("model_invalid_item".into());
                            }
                            if item["type"] == "message" {
                                for part in
                                    item["content"].as_array().ok_or("model_invalid_item")?
                                {
                                    if part["type"] == "output_text" {
                                        text.push_str(
                                            part["text"].as_str().ok_or("model_invalid_item")?,
                                        );
                                    }
                                    if part["type"] == "refusal" {
                                        text.push_str(
                                            part["refusal"].as_str().ok_or("model_invalid_item")?,
                                        );
                                    }
                                }
                            }
                        }
                        if !streamed_text.is_empty() && streamed_text != text {
                            return Err("model_text_mismatch".into());
                        }
                        return Ok(ModelResponse {
                            text,
                            output,
                            response_id: response["id"].as_str().map(str::to_owned),
                        });
                    }
                    _ => {}
                }
            }
        }
        Err("model_unfinished_stream".into())
    }
}
#[async_trait]
impl Model for CodexModel {
    async fn respond(&self, request: ModelRequest, events: Events) -> Result<ModelResponse> {
        let mut tokens = self.tokens.lock().await;
        if tokens.is_none() {
            *tokens = Some(self.sign_in(&events).await?);
        }
        if tokens.as_ref().unwrap().expires <= Instant::now() {
            // Take first: cancellation or failure during rotation must not reuse a spent token.
            let previous = tokens.take().unwrap();
            *tokens = Some(
                self.exchange(
                    &[
                        ("grant_type", "refresh_token"),
                        ("client_id", CLIENT_ID),
                        ("refresh_token", &previous.refresh),
                    ],
                    "refresh",
                )
                .await?,
            );
        }
        let result = self
            .request(tokens.as_ref().unwrap(), request, events)
            .await;
        // No transparent replay of a potentially started request. A new turn signs in.
        if result.as_ref().is_err_and(|e| e == "model_unauthorized") {
            *tokens = None;
        }
        result
    }
}
pub struct CodexModelPlugin(pub Config);
#[async_trait]
impl Plugin for CodexModelPlugin {
    fn id(&self) -> &'static str {
        "codex-model"
    }
    fn provides(&self) -> &'static [&'static str] {
        &["model"]
    }
    fn requires(&self) -> &'static [&'static str] {
        &[]
    }
    async fn start(&mut self, mut context: Context<'_>) -> Result<()> {
        context.register(
            "model",
            ModelService(Arc::new(CodexModel::new(self.0.clone())?)),
        )
    }
    async fn close(&mut self) -> Result<()> {
        Ok(())
    }
}
fn random() -> Result<String> {
    let mut bytes = [0u8; 32];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut bytes))
        .map_err(|_| "sign_in_random_failed")?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}
async fn callback(listener: TcpListener, state: &str) -> Result<String> {
    let (mut socket, _) = listener
        .accept()
        .await
        .map_err(|_| "sign_in_callback_failed")?;
    let result = tokio::time::timeout(Duration::from_secs(5), async {
        let mut request = Vec::new();
        let mut buf = [0u8; 1024];
        while !request.windows(4).any(|w| w == b"\r\n\r\n") {
            let n = socket
                .read(&mut buf)
                .await
                .map_err(|_| "sign_in_callback_failed")?;
            if n == 0 || request.len() + n > 8192 {
                return Err("sign_in_callback_failed".to_owned());
            }
            request.extend_from_slice(&buf[..n]);
        }
        let request = std::str::from_utf8(&request).map_err(|_| "sign_in_callback_failed")?;
        let parts: Vec<_> = request
            .lines()
            .next()
            .unwrap_or("")
            .split_whitespace()
            .collect();
        if parts.len() != 3 || parts[0] != "GET" || !parts[1].starts_with("/auth/callback?") {
            return Err("sign_in_callback_failed".into());
        }
        let url = Url::parse(&format!("http://localhost{}", parts[1]))
            .map_err(|_| "sign_in_callback_failed")?;
        let query: Vec<_> = url.query_pairs().collect();
        let states: Vec<_> = query.iter().filter(|(k, _)| k == "state").collect();
        if states.len() != 1 || states[0].1 != state {
            return Err("sign_in_state_mismatch".into());
        }
        if query.iter().any(|(k, _)| k == "error") {
            return Err("sign_in_denied".into());
        }
        let codes: Vec<_> = query.iter().filter(|(k, _)| k == "code").collect();
        if codes.len() != 1 || codes[0].1.is_empty() {
            return Err("sign_in_callback_failed".into());
        }
        Ok(codes[0].1.to_string())
    })
    .await
    .unwrap_or_else(|_| Err("sign_in_callback_failed".into()));
    let status = if result.is_ok() {
        "200 OK"
    } else {
        "400 Bad Request"
    };
    let body = if result.is_ok() {
        "Sign-in received. Return to Psi."
    } else {
        "Sign-in callback rejected. Return to Psi and retry."
    };
    let reply = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/plain\r\nCache-Control: no-store\r\nReferrer-Policy: no-referrer\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    );
    let _ = tokio::time::timeout(Duration::from_secs(2), socket.write_all(reply.as_bytes())).await;
    result
}
async fn limited_json(response: reqwest::Response) -> Result<Value> {
    let mut bytes = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| "token_transport")?;
        if bytes.len() + chunk.len() > 65536 {
            return Err("token_size".into());
        }
        bytes.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&bytes).map_err(|_| "token_json".into())
}
fn parse_tokens(value: Value) -> Result<Tokens> {
    let field = |key: &str| {
        value[key]
            .as_str()
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .ok_or_else(|| "token_field".to_owned())
    };
    let access = field("access_token")?;
    let refresh = field("refresh_token")?;
    let seconds = value["expires_in"]
        .as_u64()
        .filter(|s| *s > 0 && *s <= 31_536_000)
        .ok_or("token_expiry")?;
    // Routing metadata received over TLS, not a locally verified identity assertion.
    let claims: Value = serde_json::from_slice(
        &URL_SAFE_NO_PAD
            .decode(access.split('.').nth(1).ok_or("token_claims")?)
            .map_err(|_| "token_claims")?,
    )
    .map_err(|_| "token_claims")?;
    let account = claims["https://api.openai.com/auth"]["chatgpt_account_id"]
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or("token_account")?
        .to_owned();
    Ok(Tokens {
        access,
        refresh,
        account,
        expires: Instant::now() + Duration::from_secs(seconds.saturating_sub(30)),
    })
}

/// Byte-oriented SSE decoder: UTF-8 is decoded only after a full line arrives.
#[derive(Default)]
struct Sse {
    line: Vec<u8>,
    data: Vec<String>,
    total: usize,
    after_cr: bool,
    first: bool,
}
impl Sse {
    fn chunk(&mut self, bytes: &[u8]) -> Result<Vec<Value>> {
        self.total += bytes.len();
        if self.total > 512 * 1024 {
            return Err("model_stream_limit".into());
        }
        let mut events = Vec::new();
        for &byte in bytes {
            if self.after_cr && byte == b'\n' {
                self.after_cr = false;
                continue;
            }
            self.after_cr = byte == b'\r';
            if byte == b'\r' || byte == b'\n' {
                let line = std::str::from_utf8(&self.line).map_err(|_| "model_invalid_utf8")?;
                let line = if !self.first {
                    self.first = true;
                    line.strip_prefix('\u{feff}').unwrap_or(line)
                } else {
                    line
                };
                if line.is_empty() {
                    if !self.data.is_empty() {
                        let data = self.data.join("\n");
                        self.data.clear();
                        if data != "[DONE]" {
                            events.push(
                                serde_json::from_str(&data).map_err(|_| "model_invalid_json")?,
                            );
                        }
                    }
                } else if let Some(data) = line.strip_prefix("data:") {
                    self.data
                        .push(data.strip_prefix(' ').unwrap_or(data).into());
                }
                self.line.clear();
            } else {
                self.line.push(byte);
            }
        }
        Ok(events)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn every_utf8_and_crlf_split_is_lossless() {
        let wire =
            "\u{feff}: comment\r\ndata: {\r\ndata: \"delta\":\"Привет 🌍\"}\r\n\r\n".as_bytes();
        for split in 0..=wire.len() {
            let mut parser = Sse::default();
            let mut values = parser.chunk(&wire[..split]).unwrap();
            values.extend(parser.chunk(&wire[split..]).unwrap());
            assert_eq!(values, vec![json!({"delta":"Привет 🌍"})]);
        }
    }
}

#[cfg(test)]
mod proxy_tests {
    use super::*;
    use std::io::{Read, Write};

    // A child test process isolates proxy environment variables from parallel tests.
    // Both destinations are local listeners: even a regression cannot call OpenAI.
    #[test]
    fn live_proxy_and_fixture_bypass() {
        const MARKER: &str = "PSI_PROXY_REGRESSION_ORIGIN";
        if let Ok(origin) = std::env::var(MARKER) {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(async {
                let live = CodexModel::new(Config::live("unused".into())).unwrap();
                let fixture =
                    CodexModel::new(Config::fixture(&origin, "unused".into()).unwrap()).unwrap();
                for model in [live, fixture] {
                    let response = model
                        .client
                        .get(format!("{origin}/probe"))
                        .timeout(Duration::from_secs(3))
                        .send()
                        .await
                        .unwrap();
                    assert_eq!(response.status(), 200);
                }
            });
            return;
        }
        fn peer() -> (String, std::thread::JoinHandle<Option<String>>) {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            listener.set_nonblocking(true).unwrap();
            let url = format!("http://{}", listener.local_addr().unwrap());
            let task = std::thread::spawn(move || {
                let deadline = Instant::now() + Duration::from_secs(5);
                while Instant::now() < deadline {
                    match listener.accept() {
                        Ok((mut stream, _)) => {
                            stream
                                .set_read_timeout(Some(Duration::from_secs(2)))
                                .unwrap();
                            let mut request = Vec::new();
                            let mut bytes = [0u8; 1024];
                            while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                                let n = stream.read(&mut bytes).unwrap();
                                assert!(n > 0 && request.len() + n < 8192);
                                request.extend_from_slice(&bytes[..n]);
                            }
                            stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
                            return Some(
                                String::from_utf8(request)
                                    .unwrap()
                                    .lines()
                                    .next()
                                    .unwrap()
                                    .into(),
                            );
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            std::thread::sleep(Duration::from_millis(10))
                        }
                        Err(e) => panic!("{e}"),
                    }
                }
                None
            });
            (url, task)
        }
        let (origin, direct) = peer();
        let (proxy_url, proxy) = peer();
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "codex::proxy_tests::live_proxy_and_fixture_bypass",
                "--nocapture",
            ])
            .env(MARKER, &origin)
            .env("HTTP_PROXY", &proxy_url)
            .env("http_proxy", &proxy_url)
            .env("HTTPS_PROXY", &proxy_url)
            .env("https_proxy", &proxy_url)
            .env("ALL_PROXY", &proxy_url)
            .env("all_proxy", &proxy_url)
            .env_remove("NO_PROXY")
            .env_remove("no_proxy")
            .output()
            .unwrap();
        let proxy_request = proxy.join().unwrap();
        let direct_request = direct.join().unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(proxy_request, Some(format!("GET {origin}/probe HTTP/1.1")));
        assert_eq!(direct_request.as_deref(), Some("GET /probe HTTP/1.1"));
    }
}
