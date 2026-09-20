//! Bend decides admission. Rust binds its answer to immutable work and serializes
//! single-use handoff against closure; it never substitutes a policy decision.
use crate::core::{Context, Plugin};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    sync::{
        Arc, Mutex, Weak,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};
use tokio::{
    io::AsyncReadExt,
    sync::{oneshot, watch},
    task::JoinHandle,
};

pub const COMPILER: &str = "15ae0c86f3193b8f645b4bedbc438655b648d0da";
const DEADLINE: Duration = Duration::from_millis(250);
const SOURCES: &[(&str, &str)] = &[
    ("Policy.bend", include_str!("../policy/Policy.bend")),
    ("LAWS.bend", include_str!("../policy/LAWS.bend")),
    ("PROOF.bend", include_str!("../policy/PROOF.bend")),
    ("Main.bend", include_str!("../policy/Main.bend")),
];

#[derive(Clone, Debug)]
pub struct Config {
    pub bundle: PathBuf,
    /// Explicit startup-only fault injection; never selected by model text.
    pub fault: Option<String>,
    /// Hold a valid response before handoff for controlled cancellation tests.
    pub gate: Option<PathBuf>,
}
impl Default for Config {
    fn default() -> Self {
        Self {
            bundle: PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("policy/.build"),
            fault: None,
            gate: None,
        }
    }
}
#[derive(Clone)]
pub struct PolicyService {
    backend: Arc<Backend>,
    epoch: String,
    counter: Arc<AtomicU64>,
    gates: Arc<Mutex<Vec<Weak<Gate>>>>,
}
struct Backend {
    config: Config,
    executable: Result<std::fs::File, String>,
}
impl Backend {
    fn new(config: Config) -> Self {
        let executable = checked_executable(&config);
        Self { config, executable }
    }
}
pub struct PolicyPlugin {
    config: Config,
    service: Option<PolicyService>,
}
impl PolicyPlugin {
    pub fn new(config: Config) -> Self {
        Self {
            config,
            service: None,
        }
    }
}
#[async_trait]
impl Plugin for PolicyPlugin {
    fn id(&self) -> &'static str {
        "bend-policy"
    }
    fn provides(&self) -> &'static [&'static str] {
        &["policy"]
    }
    fn requires(&self) -> &'static [&'static str] {
        &[]
    }
    async fn start(&mut self, mut context: Context<'_>) -> Result<(), String> {
        let service = PolicyService::new(self.config.clone());
        self.service = Some(service.clone());
        context.register("policy", service)
    }
    async fn close(&mut self) -> Result<(), String> {
        if let Some(service) = self.service.take() {
            let gates: Vec<_> = service
                .gates
                .lock()
                .unwrap()
                .iter()
                .filter_map(Weak::upgrade)
                .collect();
            for gate in &gates {
                gate.close();
            }
            let mut errors = vec![];
            for gate in gates {
                if let Err(error) = gate.cleanup(Event::Cancel).await {
                    errors.push(error);
                }
            }
            if !errors.is_empty() {
                return Err(errors.join("; "));
            }
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum State {
    Active,
    Cancelled,
    Finished,
}
#[derive(Clone, Copy, Debug)]
pub enum Event {
    Request,
    Cancel,
    Finish,
    Late,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Action {
    id: String,
    name: String,
    arguments: Value,
}
impl Action {
    pub fn new(name: &str, arguments: Value) -> Self {
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            name: name.into(),
            arguments,
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
struct Identity {
    request: String,
    turn: String,
    action: Action,
    epoch: String,
}
pub struct Permit {
    identity: Identity,
    generation: u64,
}
struct Inner {
    state: State,
    closed: bool,
    generation: u64,
    issued: HashMap<String, Action>,
    actions: HashSet<String>,
    pending: bool,
    handoffs: u64,
    trace: Vec<Value>,
    terminal: bool,
}
pub struct Gate {
    service: PolicyService,
    turn: String,
    inner: Mutex<Inner>,
    cancelled: watch::Sender<bool>,
    // Owned separately from the abortable agent task so cancellation can reap it.
    workers: Mutex<Vec<JoinHandle<()>>>,
    cleanup_lock: tokio::sync::Mutex<()>,
}
impl PolicyService {
    fn new(config: Config) -> Self {
        Self {
            backend: Arc::new(Backend::new(config)),
            epoch: uuid::Uuid::new_v4().to_string(),
            counter: Arc::new(AtomicU64::new(0)),
            gates: Arc::new(Mutex::new(vec![])),
        }
    }
    pub fn turn(&self, turn: &str) -> Arc<Gate> {
        let (cancelled, _) = watch::channel(false);
        let gate = Arc::new(Gate {
            service: self.clone(),
            turn: turn.into(),
            inner: Mutex::new(Inner {
                state: State::Active,
                closed: false,
                generation: 0,
                issued: HashMap::new(),
                actions: HashSet::new(),
                pending: false,
                handoffs: 0,
                trace: vec![],
                terminal: false,
            }),
            cancelled,
            workers: Mutex::new(vec![]),
            cleanup_lock: tokio::sync::Mutex::new(()),
        });
        let mut gates = self.gates.lock().unwrap();
        gates.retain(|gate| gate.strong_count() > 0);
        gates.push(Arc::downgrade(&gate));
        gate
    }
}
impl Gate {
    /// Synchronous barrier shared with handoff. No subprocess/await on this path.
    pub fn close(&self) {
        let mut inner = self.inner.lock().unwrap();
        inner.closed = true;
        inner.generation += 1;
        inner.issued.clear();
        self.cancelled.send_replace(true);
    }
    pub fn evidence(&self) -> Value {
        let inner = self.inner.lock().unwrap();
        json!({"epoch":self.service.epoch,"turn":self.turn,"state":inner.state,"closed":inner.closed,"handoffs":inner.handoffs,"runtime_counter":self.service.counter.load(Ordering::SeqCst),"transitions":inner.trace})
    }
    pub async fn cleanup(&self, event: Event) -> Result<(), String> {
        self.close();
        let _cleanup = self.cleanup_lock.lock().await;
        if self.inner.lock().unwrap().terminal {
            return Ok(());
        }
        let workers = std::mem::take(&mut *self.workers.lock().unwrap());
        for worker in workers {
            worker.await.map_err(|_| "policy_worker_failed")?;
        }
        // Work is already closed and the request child reaped. A failed terminal
        // transition cannot reopen admission or prevent cleanup of active work.
        let action = Action::new("lifecycle", json!({"event":format!("{event:?}")}));
        let identity = self.identity(action);
        let state = self.inner.lock().unwrap().state;
        let (_keep_sender, receiver) = watch::channel(false);
        let result = transition(&self.service.backend, &identity, state, event, receiver).await;
        self.record(&identity, event, &result);
        if let Ok((next, _)) = result {
            self.inner.lock().unwrap().state = next;
        }
        self.inner.lock().unwrap().terminal = true;
        if self
            .inner
            .lock()
            .unwrap()
            .trace
            .iter()
            .any(|entry| entry["result"]["Err"] == "policy_cleanup_failed")
        {
            return Err("policy_cleanup_failed".into());
        }
        Ok(())
    }
    fn identity(&self, action: Action) -> Identity {
        Identity {
            request: uuid::Uuid::new_v4().to_string(),
            turn: self.turn.clone(),
            action,
            epoch: self.service.epoch.clone(),
        }
    }
    fn record(&self, identity: &Identity, event: Event, result: &Result<(State, bool), String>) {
        self.inner.lock().unwrap().trace.push(json!({"request_id":identity.request,"turn_id":identity.turn,"action":identity.action,"epoch":identity.epoch,"event":format!("{event:?}"),"result":result}));
    }
    pub async fn request(self: &Arc<Self>, action: Action) -> Result<Permit, String> {
        let identity = self.identity(action);
        let (tx, rx) = oneshot::channel();
        {
            let mut inner = self.inner.lock().unwrap();
            if inner.closed {
                return Err("policy_admission_closed".into());
            }
            if inner.pending || !inner.issued.is_empty() {
                return Err("policy_busy".into());
            }
            if !inner.actions.insert(identity.action.id.clone()) {
                return Err("policy_action_reused".into());
            }
            inner.pending = true;
            let state = inner.state;
            let generation = inner.generation;
            let gate = self.clone();
            let cancelled = self.cancelled.subscribe();
            let worker = tokio::spawn(async move {
                let result = transition(
                    &gate.service.backend,
                    &identity,
                    state,
                    Event::Request,
                    cancelled,
                )
                .await;
                gate.record(&identity, Event::Request, &result);
                let mut inner = gate.inner.lock().unwrap();
                inner.pending = false;
                let permit = if inner.closed || inner.generation != generation {
                    Err("policy_stale_response".into())
                } else {
                    match result {
                        Ok((next, decision)) => {
                            inner.state = next;
                            if decision {
                                inner
                                    .issued
                                    .insert(identity.request.clone(), identity.action.clone());
                                Ok(Permit {
                                    identity,
                                    generation,
                                })
                            } else {
                                Err("policy_denied".into())
                            }
                        }
                        Err(error) => {
                            inner.closed = true;
                            inner.generation += 1;
                            Err(error)
                        }
                    }
                };
                let _ = tx.send(permit);
            });
            self.workers.lock().unwrap().push(worker);
        }
        rx.await.map_err(|_| "policy_worker_failed")?
    }
    /// The executor receives exactly the immutable admitted action. No await or
    /// cancellable gap between consuming authorization and this harmless effect.
    pub fn handoff<T>(
        &self,
        permit: Permit,
        action: &Action,
        execute: impl FnOnce() -> Result<T, String>,
    ) -> Result<T, String> {
        let mut inner = self.inner.lock().unwrap();
        if inner.closed
            || inner.generation != permit.generation
            || permit.identity.turn != self.turn
            || permit.identity.epoch != self.service.epoch
            || &permit.identity.action != action
            || inner.issued.get(&permit.identity.request) != Some(action)
        {
            return Err("policy_invalid_permit".into());
        }
        inner.issued.remove(&permit.identity.request);
        inner.handoffs += 1;
        execute()
    }
    pub fn fixture_handoff(&self, permit: Permit, action: &Action) -> Result<u64, String> {
        if action.name != "fixture_counter" || action.arguments != json!({"increment":1}) {
            return Err("fixture_action_unsupported".into());
        }
        self.handoff(permit, action, || {
            Ok(self.service.counter.fetch_add(1, Ordering::SeqCst) + 1)
        })
    }
    pub async fn fixture(
        self: &Arc<Self>,
        events: &crate::model::Events,
    ) -> Result<(u64, &'static str), String> {
        let action = Action::new("fixture_counter", json!({"increment":1}));
        let permit = self.request(action.clone()).await?;
        let prefix = if self.service.backend.config.gate.is_some() {
            "Psi policy: Permit received; waiting for fixture release.\n"
        } else {
            ""
        };
        if let Some(path) = &self.service.backend.config.gate {
            let _ = events.send(crate::model::ModelEvent::Text {
                delta: prefix.into(),
            });
            while !path.exists() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        }
        self.fixture_handoff(permit, &action)
            .map(|count| (count, prefix))
    }
}

fn checked_executable(config: &Config) -> Result<std::fs::File, String> {
    use std::io::Write;
    use std::os::fd::{AsRawFd, FromRawFd};
    let check = || -> Result<std::fs::File, Box<dyn std::error::Error>> {
        let manifest: Value =
            serde_json::from_slice(&std::fs::read(config.bundle.join("manifest.json"))?)?;
        if manifest["compiler_commit"] != COMPILER
            || manifest["proof_stdout"].as_str().map(str::trim) != Some("All terms check.")
            || manifest["proof_stderr"] != ""
            || manifest["mutation_exit"]
                .as_i64()
                .is_none_or(|code| code == 0)
        {
            return Err("invalid proof manifest".into());
        }
        for (name, source) in SOURCES {
            if manifest["source_sha256"][*name]
                != format!("{:x}", Sha256::digest(source.as_bytes()))
            {
                return Err("policy source mismatch".into());
            }
        }
        let bytes = std::fs::read(config.bundle.join("policy"))?;
        if manifest["binary_sha256"] != format!("{:x}", Sha256::digest(&bytes)) {
            return Err("policy binary mismatch".into());
        }
        // Execute the exact hashed bytes, not a pathname that can change between
        // verification and exec. This is Linux-only, matching the MVP platform.
        let fd = unsafe {
            libc::memfd_create(
                c"psi-policy".as_ptr(),
                libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING,
            )
        };
        if fd < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        let mut file = unsafe { std::fs::File::from_raw_fd(fd) };
        file.write_all(&bytes)?;
        if unsafe {
            libc::fcntl(
                file.as_raw_fd(),
                libc::F_ADD_SEALS,
                libc::F_SEAL_WRITE | libc::F_SEAL_GROW | libc::F_SEAL_SHRINK | libc::F_SEAL_SEAL,
            )
        } < 0
        {
            return Err(std::io::Error::last_os_error().into());
        }
        Ok(file)
    };
    check().map_err(|e| format!("policy_unavailable: {e}"))
}

async fn bounded(mut pipe: impl tokio::io::AsyncRead + Unpin) -> Result<Vec<u8>, String> {
    let mut bytes = vec![];
    (&mut pipe)
        .take(4097)
        .read_to_end(&mut bytes)
        .await
        .map_err(|_| "policy_pipe_failed")?;
    if bytes.len() > 4096 {
        return Err("policy_output_limit".into());
    }
    Ok(bytes)
}
async fn transition(
    backend: &Backend,
    identity: &Identity,
    state: State,
    event: Event,
    mut cancelled: watch::Receiver<bool>,
) -> Result<(State, bool), String> {
    use std::os::fd::AsRawFd;
    use std::process::Stdio;
    let config = &backend.config;
    let start = tokio::time::Instant::now();
    if *cancelled.borrow() {
        return Err("policy_cancelled".into());
    }
    if config.fault.as_deref() == Some("missing") {
        return Err("policy_unavailable: injected missing executable".into());
    }
    let executable = backend.executable.as_ref().map_err(Clone::clone)?;
    let mut command =
        tokio::process::Command::new(format!("/proc/self/fd/{}", executable.as_raw_fd()));
    command.args(["--threads", "1", "--gpu", "off", "--"]);
    command.args([
        identity.request.as_str(),
        &identity.turn,
        &identity.action.id,
        &identity.epoch,
        &format!("{state:?}"),
        &format!("{event:?}"),
    ]);
    if config.fault.as_deref() == Some("failed") {
        command.args(["unexpected-extra-argument"]);
    }
    command
        .env_clear()
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = command
        .spawn()
        .map_err(|e| format!("policy_launch_failed: {e}"))?;
    let pid = child.id().unwrap();
    eprintln!("Psi policy child: {pid}");
    if config.fault.as_deref() == Some("hang") {
        unsafe {
            libc::kill(pid as i32, libc::SIGSTOP);
        }
    }
    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();
    let result = tokio::select! {
        biased;
        _ = async { if !*cancelled.borrow() { let _ = cancelled.changed().await; } } => Err("policy_cancelled".into()),
        _ = tokio::time::sleep_until(start + DEADLINE) => Err("policy_timeout".into()),
        output = async { tokio::try_join!(bounded(stdout), bounded(stderr), async { child.wait().await.map_err(|_| "policy_wait_failed".to_string()) }) } => output,
    };
    // Always reap before exposing a result or allowing the slot to be released.
    if let Err(error) = &result {
        let _ = child.start_kill();
        tokio::time::timeout(Duration::from_secs(2), child.wait())
            .await
            .map_err(|_| "policy_cleanup_failed")?
            .map_err(|_| "policy_cleanup_failed")?;
        eprintln!("Psi policy reaped: {pid}");
        return Err(error.clone());
    }
    eprintln!("Psi policy reaped: {pid}");
    let (mut stdout, stderr, status) = result?;
    if !status.success() {
        return Err("policy_process_failed".into());
    }
    if !stderr.is_empty() {
        return Err("policy_invalid_response".into());
    }
    if config.fault.as_deref() == Some("invalid") {
        stdout = b"invalid\n".to_vec();
    }
    if config.fault.as_deref() == Some("mismatch") {
        stdout = String::from_utf8_lossy(&stdout)
            .replace(&identity.request, "wrong-request")
            .into_bytes();
    }
    let text = std::str::from_utf8(&stdout).map_err(|_| "policy_invalid_response")?;
    let fields: Vec<_> = text
        .strip_suffix('\n')
        .ok_or("policy_invalid_response")?
        .split('\t')
        .collect();
    if fields.len() != 7 {
        return Err("policy_invalid_response".into());
    }
    if fields[..5]
        != [
            "B1",
            &identity.request,
            &identity.turn,
            &identity.action.id,
            &identity.epoch,
        ]
    {
        return Err("policy_identity_mismatch".into());
    }
    let state = match fields[5] {
        "Active" => State::Active,
        "Cancelled" => State::Cancelled,
        "Finished" => State::Finished,
        _ => return Err("policy_invalid_response".into()),
    };
    let permit = match fields[6] {
        "0" => false,
        "1" => true,
        _ => return Err("policy_invalid_response".into()),
    };
    Ok((state, permit))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn service(config: Config) -> PolicyService {
        PolicyService::new(config)
    }
    fn action() -> Action {
        Action::new("fixture_counter", json!({"increment":1}))
    }
    fn copy_permit(p: &Permit) -> Permit {
        Permit {
            identity: p.identity.clone(),
            generation: p.generation,
        }
    }
    #[tokio::test]
    async fn real_cpu_all_twelve_transitions() {
        let backend = Backend::new(Config::default());
        assert!(
            backend.executable.is_ok(),
            "run python3 policy/build.py before policy tests"
        );
        for state in [State::Active, State::Cancelled, State::Finished] {
            for event in [Event::Request, Event::Cancel, Event::Finish, Event::Late] {
                let identity = Identity {
                    request: "req".into(),
                    turn: "turn".into(),
                    action: action(),
                    epoch: "epoch".into(),
                };
                let (_sender, receiver) = watch::channel(false);
                let expected = match (state, event) {
                    (State::Active, Event::Request) => (State::Active, true),
                    (State::Active, Event::Cancel) => (State::Cancelled, false),
                    (State::Active, Event::Finish) => (State::Finished, false),
                    _ => (state, false),
                };
                assert_eq!(
                    transition(&backend, &identity, state, event, receiver)
                        .await
                        .unwrap(),
                    expected
                );
            }
        }
    }
    #[tokio::test]
    async fn permits_bind_identity_arguments_epoch_and_single_use() {
        let service = service(Config::default());
        let gate = service.turn("t1");
        let work = action();
        let permit = gate.request(work.clone()).await.unwrap();
        let duplicate = copy_permit(&permit);
        let mut wrong_request = copy_permit(&permit);
        wrong_request.identity.request = "wrong-request".into();
        assert!(gate.fixture_handoff(wrong_request, &work).is_err());
        assert!(
            service
                .turn("t1")
                .fixture_handoff(copy_permit(&permit), &work)
                .is_err()
        );
        assert!(gate.request(action()).await.is_err());
        let mut changed = work.clone();
        changed.arguments = json!({"increment":2});
        assert!(
            gate.fixture_handoff(copy_permit(&permit), &changed)
                .is_err()
        );
        changed = work.clone();
        changed.id = "another-action".into();
        assert!(
            gate.fixture_handoff(copy_permit(&permit), &changed)
                .is_err()
        );
        assert!(
            service
                .turn("t2")
                .fixture_handoff(copy_permit(&permit), &work)
                .is_err()
        );
        let restarted = PolicyService {
            backend: service.backend.clone(),
            epoch: "new-epoch".into(),
            counter: Arc::new(AtomicU64::new(0)),
            gates: Arc::new(Mutex::new(vec![])),
        };
        assert!(
            restarted
                .turn("t1")
                .fixture_handoff(copy_permit(&permit), &work)
                .is_err()
        );
        assert_eq!(gate.fixture_handoff(permit, &work).unwrap(), 1);
        assert!(gate.fixture_handoff(duplicate, &work).is_err());
        assert!(gate.request(work).await.is_err());
        assert_eq!(gate.evidence()["handoffs"], 1);
        gate.close();
        gate.cleanup(Event::Finish).await.unwrap();
        assert_eq!(gate.evidence()["state"], "Finished");
        assert!(gate.request(action()).await.is_err());
    }
    #[tokio::test]
    async fn cancellation_orders_before_request_between_decision_and_handoff_and_after_effect() {
        let service = service(Config::default());
        let before = service.turn("before");
        before.close();
        assert!(before.request(action()).await.is_err());
        before.cleanup(Event::Cancel).await.unwrap();
        assert_eq!(before.evidence()["state"], "Cancelled");
        let between = service.turn("between");
        let work = action();
        let permit = between.request(work.clone()).await.unwrap();
        between.close();
        assert!(between.fixture_handoff(permit, &work).is_err());
        between.cleanup(Event::Cancel).await.unwrap();
        assert_eq!(between.evidence()["handoffs"], 0);
        let after = service.turn("after");
        let work = action();
        let permit = after.request(work.clone()).await.unwrap();
        after.fixture_handoff(permit, &work).unwrap();
        after.close();
        after.cleanup(Event::Cancel).await.unwrap();
        assert_eq!(after.evidence()["handoffs"], 1);
        assert!(after.request(action()).await.is_err());
    }
    #[tokio::test]
    async fn failures_and_cancellation_while_waiting_never_handoff() {
        for (fault, expected) in [
            ("missing", "policy_unavailable"),
            ("hang", "policy_timeout"),
            ("invalid", "policy_invalid_response"),
            ("mismatch", "policy_identity_mismatch"),
            ("failed", "policy_process_failed"),
        ] {
            let gate = service(Config {
                fault: Some(fault.into()),
                ..Config::default()
            })
            .turn(fault);
            let started = std::time::Instant::now();
            let error = gate.request(action()).await.err().expect("fault must deny");
            assert!(error.starts_with(expected), "{fault}: {error}");
            assert!(started.elapsed() < Duration::from_secs(2));
            assert_eq!(gate.evidence()["handoffs"], 0);
            gate.close();
            gate.cleanup(Event::Cancel).await.unwrap();
        }
        let gate = service(Config {
            fault: Some("hang".into()),
            ..Config::default()
        })
        .turn("cancel-pending");
        let requester = gate.clone();
        let task = tokio::spawn(async move { requester.request(action()).await });
        tokio::time::sleep(Duration::from_millis(20)).await;
        let started = std::time::Instant::now();
        gate.close();
        assert!(task.await.unwrap().is_err());
        assert!(
            started.elapsed() < Duration::from_millis(200),
            "cancellation must not wait for the 250ms decision deadline"
        );
        gate.cleanup(Event::Cancel).await.unwrap();
        assert_eq!(gate.evidence()["handoffs"], 0);
    }
    #[tokio::test]
    async fn malformed_bundle_and_missing_binary_fail_closed() {
        let root = std::env::temp_dir().join(format!("psi-policy-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        let config = Config {
            bundle: root.clone(),
            ..Config::default()
        };
        assert!(
            service(config.clone())
                .turn("missing")
                .request(action())
                .await
                .is_err()
        );
        std::fs::copy(
            Config::default().bundle.join("manifest.json"),
            root.join("manifest.json"),
        )
        .unwrap();
        std::fs::write(root.join("policy"), b"not the checked program").unwrap();
        let gate = service(config).turn("tampered");
        assert!(
            gate.request(action())
                .await
                .err()
                .unwrap()
                .contains("binary mismatch")
        );
        assert_eq!(gate.evidence()["handoffs"], 0);
        std::fs::remove_dir_all(root).unwrap();
    }
}
