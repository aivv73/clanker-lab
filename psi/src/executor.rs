//! Linux command supervisor. The service owns cleanup independently of the agent future.
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt},
    process::Command,
    sync::watch,
};

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExecArgs {
    pub cmd: String,
    #[serde(default)]
    pub workdir: Option<String>,
    #[serde(default)]
    pub shell: Option<String>,
    #[serde(default)]
    pub login: bool,
    #[serde(default)]
    pub tty: bool,
    #[serde(default = "deadline")]
    pub timeout_ms: u64,
    #[serde(default = "yield_ms")]
    pub yield_time_ms: u64,
    #[serde(default = "tokens")]
    pub max_output_tokens: usize,
}
fn yield_ms() -> u64 {
    10_000
}
fn deadline() -> u64 {
    120_000
}
fn tokens() -> usize {
    16_000
}
impl ExecArgs {
    pub fn validate(&self) -> Result<(), String> {
        if self.cmd.is_empty()
            || self.cmd.len() > 65536
            || self.cmd.contains('\0')
            || self.login
            || self.tty
            || self.shell.as_deref().is_some_and(|s| s != "/bin/sh")
            || self.yield_time_ms > 30_000
            || self.timeout_ms == 0
            || self.timeout_ms > 600_000
            || self.max_output_tokens == 0
            || self.max_output_tokens > 16_000
        {
            return Err("unsupported_exec_arguments".into());
        }
        if let Some(dir) = &self.workdir {
            let p = Path::new(dir);
            if !p.starts_with("/workspace")
                || p.components()
                    .any(|c| matches!(c, std::path::Component::ParentDir))
            {
                return Err("unsupported_workdir".into());
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WriteArgs {
    pub session_id: String,
    #[serde(default)]
    pub chars: String,
    #[serde(default = "yield_ms")]
    pub yield_time_ms: u64,
    #[serde(default = "tokens")]
    pub max_output_tokens: usize,
}
impl WriteArgs {
    fn validate(&self) -> Result<(), String> {
        if self.chars.len() > 65536
            || self.yield_time_ms > 30000
            || self.max_output_tokens == 0
            || self.max_output_tokens > 16000
        {
            return Err("unsupported_write_arguments".into());
        }
        Ok(())
    }
}
type Reply = tokio::sync::oneshot::Sender<Result<Value, String>>;
struct Request {
    arguments: Value,
    id: String,
    reply: Reply,
}
struct Running {
    id: String,
    requests: tokio::sync::mpsc::UnboundedSender<Request>,
    terminal: Arc<Mutex<Option<Result<Value, String>>>>,
}
pub struct ToolTurn {
    workspace: PathBuf,
    directory: PathBuf,
    session: String,
    turn: String,
    cancel: watch::Sender<bool>,
    running: Mutex<Option<Running>>,
    invocation: tokio::sync::Mutex<()>,
    worker: Mutex<Vec<tokio::task::JoinHandle<Result<Value, String>>>>,
}
impl ToolTurn {
    pub fn new(workspace: PathBuf, directory: PathBuf, session: String, turn: String) -> Arc<Self> {
        Arc::new(Self {
            workspace,
            directory,
            session,
            turn,
            cancel: watch::channel(false).0,
            running: Mutex::new(None),
            invocation: tokio::sync::Mutex::new(()),
            worker: Mutex::new(Vec::new()),
        })
    }
    pub async fn execute(
        self: &Arc<Self>,
        arguments: Value,
        gate: Arc<crate::policy::Gate>,
    ) -> Result<Value, String> {
        self.invoke("exec_command", arguments, gate).await
    }
    pub async fn write_stdin(
        self: &Arc<Self>,
        arguments: Value,
        gate: Arc<crate::policy::Gate>,
    ) -> Result<Value, String> {
        self.invoke("write_stdin", arguments, gate).await
    }
    pub async fn invoke(
        self: &Arc<Self>,
        name: &str,
        arguments: Value,
        gate: Arc<crate::policy::Gate>,
    ) -> Result<Value, String> {
        self.invoke_identified(name, arguments, gate, uuid::Uuid::new_v4().to_string())
            .await
    }
    pub async fn invoke_identified(
        self: &Arc<Self>,
        name: &str,
        arguments: Value,
        gate: Arc<crate::policy::Gate>,
        call_id: String,
    ) -> Result<Value, String> {
        let _invocation = self.invocation.try_lock().map_err(|_| "tool_busy")?;
        let action = crate::policy::Action::identified(call_id.clone(), name, arguments.clone());
        let permit = gate.request(action.clone()).await?;
        let id = call_id;
        let (tx, rx) = tokio::sync::oneshot::channel();
        gate.handoff(permit, &action, || {
            let mut running = self.running.lock().unwrap();
            if name != "write_stdin" && running.is_some() {
                return Err("tool_busy".into());
            }
            match name {
                "exec_command" => {
                    let args: ExecArgs = serde_json::from_value(arguments.clone())
                        .map_err(|_| "unsupported_exec_arguments")?;
                    args.validate()?;
                    self.intent(&id, name, &arguments)?;
                    let process = uuid::Uuid::new_v4().to_string();
                    self.db()?.execute("INSERT INTO command_processes(id,session,turn,invocation) VALUES (?,?,?,?)", rusqlite::params![process,self.session,self.turn,id]).map_err(|_| "action_intent_failed")?;
                    let (requests, mut receiver) = tokio::sync::mpsc::unbounded_channel();
                    let terminal = Arc::new(Mutex::new(None));
                    *running = Some(Running {
                        id: process.clone(),
                        requests,
                        terminal: terminal.clone(),
                    });
                    let this = self.clone();
                    let request = Request {
                        arguments: arguments.clone(),
                        id: id.clone(),
                        reply: tx,
                    };
                    self.worker.lock().unwrap().push(tokio::spawn(async move {
                        let result = supervise(
                            &this,
                            &args,
                            &process,
                            &mut receiver,
                            request,
                            this.cancel.subscribe(),
                        )
                        .await;
                        let mut final_state = terminal.lock().unwrap();
                        *final_state = Some(result.clone());
                        while let Ok(request) = receiver.try_recv() {
                            let value = result.clone().and_then(|v| this.result(&request.id, v));
                            let _ = request.reply.send(value);
                        }
                        result
                    }));
                }
                "write_stdin" => {
                    let args: WriteArgs = serde_json::from_value(arguments.clone())
                        .map_err(|_| "unsupported_write_arguments")?;
                    args.validate()?;
                    let command = running
                        .as_ref()
                        .filter(|r| r.id == args.session_id)
                        .ok_or("unknown_process")?;
                    self.intent(&id, name, &arguments)?;
                    let terminal = command.terminal.lock().unwrap();
                    if let Some(result) = terminal.as_ref() {
                        let result = result.clone().and_then(|v| self.result(&id, v));
                        let _ = tx.send(result);
                    } else {
                        command
                            .requests
                            .send(Request {
                                arguments: arguments.clone(),
                                id: id.clone(),
                                reply: tx,
                            })
                            .map_err(|_| "executor_worker_failed")?;
                    }
                }
                _ => return Err("unsupported_tool".into()),
            }
            Ok(())
        })?;
        let value = rx.await.map_err(|_| "executor_worker_failed")??;
        if value["cleanup_confirmed"] == true {
            self.running.lock().unwrap().take();
        }
        Ok(value)
    }
    fn intent(&self, id: &str, name: &str, arguments: &Value) -> Result<(), String> {
        self.db()?
            .execute(
                "INSERT INTO actions(id,session,turn,intent,state) VALUES (?,?,?,?,'intent')",
                rusqlite::params![
                    id,
                    self.session,
                    self.turn,
                    json!({"name":name,"arguments":arguments}).to_string()
                ],
            )
            .map_err(|_| "action_intent_failed")?;
        Ok(())
    }
    fn result(&self, id: &str, mut value: Value) -> Result<Value, String> {
        // A process may exit between requests. Apply the current invocation's
        // response budget even when its terminal output was already buffered.
        let intent: String = self
            .db()?
            .query_row("SELECT intent FROM actions WHERE id=?", [id], |r| r.get(0))
            .map_err(|_| "action_store_failed")?;
        let intent: Value = serde_json::from_str(&intent).map_err(|_| "action_store_failed")?;
        let limit = intent["arguments"]["max_output_tokens"]
            .as_u64()
            .unwrap_or(16000) as usize;
        if let Some(output) = value["output"].as_str().filter(|s| s.len() > limit) {
            let mut end = limit;
            while !output.is_char_boundary(end) {
                end -= 1;
            }
            let output = output[..end].to_owned();
            value["output"] = json!(output);
            value["truncated"] = json!(true);
        }
        let changed = self
            .db()?
            .execute(
                "UPDATE actions SET state='result',result=? WHERE id=? AND state='intent'",
                rusqlite::params![value.to_string(), id],
            )
            .map_err(|_| "action_result_failed")?;
        if changed != 1 {
            return Err("action_state_conflict".into());
        }
        Ok(value)
    }
    fn db(&self) -> Result<rusqlite::Connection, String> {
        let db = rusqlite::Connection::open(self.directory.join("history.sqlite"))
            .map_err(|_| "action_store_failed")?;
        db.execute_batch("PRAGMA synchronous=FULL; PRAGMA busy_timeout=5000;")
            .map_err(|_| "action_store_failed")?;
        Ok(db)
    }
    pub async fn cleanup(&self) -> Result<Option<Value>, String> {
        self.cancel.send_replace(true);
        let workers = std::mem::take(&mut *self.worker.lock().unwrap());
        let mut last = None;
        for worker in workers {
            let result = worker.await.map_err(|_| "executor_cleanup_unconfirmed")??;
            if result["cleanup_confirmed"] != true {
                return Err("executor_cleanup_unconfirmed".into());
            }
            last = Some(result);
        }
        Ok(last)
    }
}

async fn supervise(
    owner: &ToolTurn,
    args: &ExecArgs,
    process: &str,
    requests: &mut tokio::sync::mpsc::UnboundedReceiver<Request>,
    initial: Request,
    mut cancel: watch::Receiver<bool>,
) -> Result<Value, String> {
    let mut pending = Some(initial);
    let result = supervise_io(owner, args, process, requests, &mut pending, &mut cancel).await;
    let result = result.and_then(|value| {
        owner
            .db()?
            .execute(
                "UPDATE command_processes SET result=? WHERE id=?",
                rusqlite::params![value.to_string(), process],
            )
            .map_err(|_| "action_result_failed")?;
        Ok(value)
    });
    if let Some(request) = pending {
        let delivered = result.clone().and_then(|v| owner.result(&request.id, v));
        let _ = request.reply.send(delivered);
    }
    result
}
async fn supervise_io(
    owner: &ToolTurn,
    args: &ExecArgs,
    process: &str,
    requests: &mut tokio::sync::mpsc::UnboundedReceiver<Request>,
    pending: &mut Option<Request>,
    cancel: &mut watch::Receiver<bool>,
) -> Result<Value, String> {
    if *cancel.borrow() {
        return Ok(json!({"reason":"cancelled","cleanup_confirmed":true,"output":""}));
    }
    let mut manager = Command::new("/usr/bin/systemctl");
    manager
        .args(["--user", "show-environment"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true);
    if !matches!(tokio::time::timeout(Duration::from_secs(2), manager.status()).await, Ok(Ok(status)) if status.success())
    {
        return Ok(
            json!({"reason":"launch_failure","error":"preflight_user_systemd","exit_code":null,"output":"","truncated":false,"cleanup_confirmed":true}),
        );
    }
    let binary = std::env::current_exe().map_err(|_| "executor_binary_missing")?;
    let mut child = Command::new("/usr/bin/systemd-run")
        .args([
            "--user",
            "--quiet",
            "--pipe",
            "--wait",
            "--collect",
            "--service-type=exec",
            "-p",
            "Delegate=memory pids",
            "-p",
            "TimeoutStopSec=4",
            "-p",
            "KillMode=control-group",
        ])
        .arg(binary)
        .arg("--exec-supervisor")
        .arg(&owner.workspace)
        .kill_on_drop(true)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|_| "preflight_user_systemd")?;
    let mut input = Some(child.stdin.take().unwrap());
    let frame = format!("{}\n", serde_json::to_string(args).unwrap());
    let sent = tokio::select! {
        result = tokio::time::timeout(Duration::from_secs(2), input.as_mut().unwrap().write_all(frame.as_bytes())) => matches!(result, Ok(Ok(()))),
        _ = cancel.changed() => false,
    };
    let mut stopping = None;
    if !sent {
        drop(input.take());
        stopping = Some(tokio::time::Instant::now() + Duration::from_secs(6));
    }
    let mut output = tokio::io::BufReader::new(child.stdout.take().unwrap()).lines();
    let deadline = tokio::time::sleep(Duration::from_millis(args.timeout_ms + 15000));
    tokio::pin!(deadline);
    loop {
        tokio::select! {
            line = output.next_line() => {
                let line = line.map_err(|_| "executor_cleanup_unconfirmed")?.ok_or("executor_cleanup_unconfirmed")?;
                let mut value: Value = serde_json::from_str(&line).map_err(|_| "executor_cleanup_unconfirmed")?;
                let running = value["reason"] == "running";
                if running { value["session_id"] = json!(process); }
                if !running {
                    drop(input.take());
                    tokio::time::timeout(Duration::from_secs(6), child.wait()).await.map_err(|_| "executor_cleanup_unconfirmed")?.map_err(|_| "executor_cleanup_unconfirmed")?;
                    return Ok(value);
                }
                if let Some(request) = pending.take() {
                    let result = owner.result(&request.id, value);
                    let _ = request.reply.send(result);
                }
            }
            Some(request) = requests.recv(), if pending.is_none() && input.is_some() => {
                let frame = format!("{}\n", request.arguments);
                *pending = Some(request);
                let sent = tokio::select! {
                    result = tokio::time::timeout(Duration::from_secs(2), input.as_mut().unwrap().write_all(frame.as_bytes())) => matches!(result, Ok(Ok(()))),
                    _ = cancel.changed() => false,
                };
                if !sent {
                    drop(input.take());
                    stopping = Some(tokio::time::Instant::now() + Duration::from_secs(6));
                }
            }
            _ = cancel.changed(), if input.is_some() => {
                drop(input.take());
                stopping = Some(tokio::time::Instant::now() + Duration::from_secs(6));
            }
            _ = async { match stopping { Some(until) => tokio::time::sleep_until(until).await, None => std::future::pending().await } } => return Err("executor_cleanup_unconfirmed".into()),
            _ = &mut deadline => return Err("executor_cleanup_unconfirmed".into()),
        }
    }
}

/// Trusted trampoline inside the sandbox. A launch marker precedes user code.
/// Syscall probes verify the effective filter, not merely its construction.
pub fn payload_main(command: &str) -> ! {
    use std::io::Write;
    use std::os::unix::process::CommandExt;
    let denied = unsafe {
        let socket = libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0);
        let socket_denied =
            socket == -1 && std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM);
        if socket >= 0 {
            libc::close(socket);
        }
        let ring = libc::syscall(libc::SYS_io_uring_setup, 1, std::ptr::null::<u8>());
        socket_denied
            && ring == -1
            && std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
            && libc::prctl(libc::PR_GET_SECCOMP) == 2
    };
    if !denied {
        eprintln!("preflight_seccomp");
        std::process::exit(125);
    }
    print!("PSI_PAYLOAD_STARTED\n");
    if std::io::stdout().flush().is_err() {
        std::process::exit(125);
    }
    let error = std::process::Command::new("/bin/sh")
        .args(["-c", command])
        .exec();
    eprintln!("payload_shell_exec_failed: {error}");
    std::process::exit(126)
}

/// Private process entry point, always called through a delegated user service.
pub fn supervisor_main(workspace: &Path) {
    let result = linux::run(workspace).unwrap_or_else(|error| json!({"reason":"launch_failure","error":error,"exit_code":null,"cleanup_confirmed":true,"output":"","truncated":false}));
    println!("{result}");
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
mod linux {
    use super::*;
    use std::{
        fs::{self, File},
        io::{BufRead, Read, Write},
        os::{
            fd::{AsRawFd, FromRawFd},
            unix::process::CommandExt,
        },
        process::{Command, Stdio},
        time::Instant,
    };
    type R<T> = Result<T, String>;
    fn write(path: impl AsRef<Path>, value: impl ToString) -> R<()> {
        fs::write(path, value.to_string()).map_err(|_| "preflight_cgroup_write".into())
    }
    fn populated(cg: &Path) -> R<bool> {
        Ok(fs::read_to_string(cg.join("cgroup.events"))
            .map_err(|_| "cleanup_cgroup_unreadable")?
            .lines()
            .any(|l| l == "populated 1"))
    }
    fn filter() -> R<File> {
        // seccomp_data: nr at 0, arch at 4. Reject foreign ABIs, including x32.
        let mut code: Vec<libc::sock_filter> = Vec::new();
        let mut ins = |c, jt, jf, k| code.push(libc::sock_filter { code: c, jt, jf, k });
        ins(0x20, 0, 0, 4);
        ins(0x15, 1, 0, 0xc000003e);
        ins(0x06, 0, 0, 0x80000000);
        ins(0x20, 0, 0, 0);
        ins(0x45, 0, 1, 0x40000000);
        ins(0x06, 0, 0, 0x80000000);
        // Deny all socket creation, not only AF_UNIX. Also disallow descriptor
        // theft and io_uring, which otherwise bypasses socket syscall filtering.
        for nr in [
            libc::SYS_socket,
            libc::SYS_socketpair,
            libc::SYS_io_uring_setup,
            libc::SYS_io_uring_enter,
            libc::SYS_io_uring_register,
            libc::SYS_pidfd_getfd,
            libc::SYS_ptrace,
            libc::SYS_process_vm_readv,
            libc::SYS_process_vm_writev,
        ] {
            ins(0x15, 0, 1, nr as u32);
            ins(0x06, 0, 0, 0x00050000 | libc::EPERM as u32);
        }
        ins(0x06, 0, 0, 0x7fff0000);
        let fd = unsafe { libc::memfd_create(c"psi-seccomp".as_ptr(), 0) };
        if fd < 0 {
            return Err("preflight_seccomp".into());
        }
        let mut file = unsafe { File::from_raw_fd(fd) };
        let bytes = unsafe {
            std::slice::from_raw_parts(
                code.as_ptr() as *const u8,
                code.len() * std::mem::size_of::<libc::sock_filter>(),
            )
        };
        file.write_all(bytes).map_err(|_| "preflight_seccomp")?;
        use std::io::Seek;
        file.rewind().map_err(|_| "preflight_seccomp")?;
        Ok(file)
    }
    fn sandbox(workspace: &Path, args: &ExecArgs, fd: i32) -> Command {
        let mut cmd = Command::new("/usr/bin/bwrap");
        cmd.args([
            "--unshare-all",
            "--unshare-user",
            "--disable-userns",
            "--assert-userns-disabled",
            "--new-session",
            "--die-with-parent",
            "--cap-drop",
            "ALL",
            "--clearenv",
            "--setenv",
            "PATH",
            "/usr/bin",
            "--setenv",
            "HOME",
            "/nonexistent",
            "--setenv",
            "LANG",
            "C.UTF-8",
            "--setenv",
            "TMPDIR",
            "/tmp",
            "--ro-bind",
            "/usr",
            "/usr",
        ]);
        for path in ["/bin", "/lib", "/lib64"] {
            if Path::new(path).exists() {
                cmd.arg("--symlink")
                    .arg(fs::canonicalize(path).unwrap())
                    .arg(path);
            }
        }
        cmd.arg("--ro-bind")
            .arg(std::env::current_exe().expect("supervisor binary"))
            .arg("/psi-executor");
        cmd.args([
            "--dir",
            "/etc",
            "--proc",
            "/proc",
            "--remount-ro",
            "/proc",
            "--dev",
            "/dev",
            "--remount-ro",
            "/dev",
            "--size",
            "268435456",
            "--tmpfs",
            "/tmp",
            "--bind",
        ])
        .arg(workspace)
        .arg("/workspace");
        if workspace.join(".git").exists() {
            cmd.arg("--ro-bind")
                .arg(workspace.join(".git"))
                .arg("/workspace/.git");
        }
        cmd.arg("--chdir")
            .arg(args.workdir.as_deref().unwrap_or("/workspace"))
            .args(["--remount-ro", "/", "--seccomp"])
            .arg(fd.to_string())
            .args(["/psi-executor", "--sandbox-payload", &args.cmd]);
        cmd.env_clear()
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        cmd
    }
    pub fn run(workspace: &Path) -> R<Value> {
        let mut line = String::new();
        std::io::stdin()
            .lock()
            .take(512 * 1024)
            .read_line(&mut line)
            .map_err(|_| "supervisor_input")?;
        let args: ExecArgs =
            serde_json::from_str(&line).map_err(|_| "unsupported_exec_arguments")?;
        args.validate()?;
        let lease = crate::admission::TurnLease::external()?;
        let workspace = fs::canonicalize(workspace).map_err(|_| "workspace_required")?;
        // Choosing an ancestor of runtime controls or the home secret tree is not
        // an access expansion mechanism. Ordinary repositories under home work.
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .ok_or("preflight_home")?;
        if home.starts_with(&workspace)
            || Path::new("/tmp").starts_with(&workspace)
            || ["/usr", "/etc", "/proc", "/sys", "/dev", "/run"]
                .iter()
                .any(|p| workspace.starts_with(p))
            || [".ssh", ".gnupg", ".config", ".codex", ".local"]
                .iter()
                .any(|p| workspace.starts_with(home.join(p)))
        {
            return Err("workspace_contains_host_controls_or_secrets".into());
        }
        if !workspace.is_dir() {
            return Err("workspace_required".into());
        }
        if fs::symlink_metadata(workspace.join(".git")).is_ok_and(|m| m.file_type().is_symlink()) {
            return Err("workspace_git_symlink_unsupported".into());
        }
        let membership =
            fs::read_to_string("/proc/self/cgroup").map_err(|_| "preflight_unified_cgroup")?;
        let relative = membership
            .strip_prefix("0::/")
            .filter(|s| s.lines().count() == 1)
            .ok_or("preflight_unified_cgroup")?
            .trim();
        let cg = Path::new("/sys/fs/cgroup").join(relative);
        let unit = relative
            .rsplit('/')
            .next()
            .filter(|s| s.ends_with(".service"))
            .ok_or("preflight_delegated_service")?;
        let mut query = Command::new("/usr/bin/systemctl")
            .args(["--user", "show", unit, "--property=Delegate", "--value"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|_| "preflight_user_systemd")?;
        let until = Instant::now() + Duration::from_secs(2);
        let status = loop {
            match query.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) if Instant::now() < until => std::thread::sleep(Duration::from_millis(5)),
                _ => {
                    let _ = query.kill();
                    let _ = query.wait();
                    return Err("preflight_user_systemd".into());
                }
            }
        };
        let mut delegation = String::new();
        query
            .stdout
            .take()
            .unwrap()
            .take(64)
            .read_to_string(&mut delegation)
            .map_err(|_| "preflight_delegation")?;
        if !status.success() || delegation.trim() != "yes" {
            return Err("preflight_delegation".into());
        }

        let controllers = fs::read_to_string(cg.join("cgroup.controllers"))
            .map_err(|_| "preflight_delegation")?;
        for controller in ["memory", "pids"] {
            if !controllers.split_whitespace().any(|s| s == controller) {
                return Err(format!("preflight_{controller}_controller"));
            }
        }
        fs::create_dir(cg.join("supervisor")).map_err(|_| "preflight_delegation")?;
        write(cg.join("supervisor/cgroup.procs"), std::process::id())?;
        write(cg.join("cgroup.subtree_control"), "+memory +pids")?;
        let child_cg = cg.join("command");
        fs::create_dir(&child_cg).map_err(|_| "preflight_delegation")?;
        for (name, value) in [
            ("memory.max", 2147483648u64),
            ("memory.swap.max", 0),
            ("pids.max", 128),
        ] {
            write(child_cg.join(name), value)?;
            if fs::read_to_string(child_cg.join(name))
                .map_err(|_| "preflight_resource_readback")?
                .trim()
                != value.to_string()
            {
                return Err("preflight_resource_readback".into());
            }
        }
        let kill = fs::OpenOptions::new()
            .write(true)
            .open(child_cg.join("cgroup.kill"))
            .map_err(|_| "preflight_cgroup_kill")?;
        let filter = filter()?;
        let membership = child_cg.join("cgroup.procs");
        let enter = fs::OpenOptions::new()
            .write(true)
            .open(&membership)
            .map_err(|_| "preflight_cgroup_membership")?;
        let fd = filter.as_raw_fd();
        let cg_fd = enter.as_raw_fd();
        let mut cmd = sandbox(&workspace, &args, fd);
        unsafe {
            cmd.pre_exec(move || {
                // Kernel accepts 0 as the writing process. No allocation or locks after fork.
                if libc::write(cg_fd, c"0".as_ptr() as *const _, 1) != 1 {
                    return Err(std::io::Error::last_os_error());
                }
                // Close every ambient descriptor except the seccomp program and pipes.
                // close_range is mandatory on this profile.
                if libc::syscall(libc::SYS_close_range, 3u32, u32::MAX, 4u32) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                if libc::fcntl(fd, libc::F_SETFD, 0) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        lease.record_cgroup(&child_cg)?;
        let mut control = libc::pollfd {
            fd: 0,
            events: libc::POLLIN | libc::POLLHUP,
            revents: 0,
        };
        unsafe {
            libc::poll(&mut control, 1, 0);
        }
        if control.revents != 0 {
            return Err("controller_closed_before_launch".into());
        }
        let mut child = cmd
            .spawn()
            .map_err(|_| "preflight_bubblewrap_or_membership")?;
        drop(enter);
        let mut payload_input = child.stdin.take().unwrap();
        for fd in [0, payload_input.as_raw_fd()] {
            assert_eq!(
                unsafe { libc::fcntl(fd, libc::F_SETFL, libc::O_NONBLOCK) },
                0
            );
        }
        let mut control_bytes = Vec::new();
        let mut input_bytes = Vec::new();
        let mut input_offset = 0;
        let mut controller_closed = false;
        let mut response_due = Some(Instant::now() + Duration::from_millis(args.yield_time_ms));
        let mut response_limit = args.max_output_tokens;
        let mut chunk_total = 0usize;
        let mut pipes = vec![
            (
                true,
                File::from(std::os::fd::OwnedFd::from(child.stdout.take().unwrap())),
            ),
            (
                false,
                File::from(std::os::fd::OwnedFd::from(child.stderr.take().unwrap())),
            ),
        ];
        for (_, p) in &pipes {
            // An unexpected setup failure must not turn a pipe read into a
            // blocking operation. Missing supervisor output is treated as uncertain.
            assert_eq!(
                unsafe { libc::fcntl(p.as_raw_fd(), libc::F_SETFL, libc::O_NONBLOCK) },
                0
            );
        }
        let mut pipes_ok = true;
        let marker = b"PSI_PAYLOAD_STARTED\n";
        let mut marker_bytes = Vec::new();
        let start = Instant::now();
        let mut reason = "exit";
        let mut bytes = Vec::new();
        let mut total = 0usize;
        let mut exited = None;
        let mut graceful = None;
        let mut forced = None;
        let mut cleanup_ok = false;
        loop {
            if exited.is_none() {
                match child.try_wait() {
                    Ok(status) => exited = status,
                    Err(_) => {
                        let _ = (&kill).write_all(b"1");
                        break;
                    }
                }
            }
            let mut control = libc::pollfd {
                fd: 0,
                events: libc::POLLIN | libc::POLLHUP,
                revents: 0,
            };
            unsafe {
                libc::poll(&mut control, 1, 0);
            }
            if control.revents != 0 && !controller_closed {
                let mut buffer = [0u8; 8192];
                match std::io::stdin().read(&mut buffer) {
                    Ok(0) => controller_closed = true,
                    Ok(n) => control_bytes.extend_from_slice(&buffer[..n]),
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                    Err(_) => controller_closed = true,
                }
                if control_bytes.len() > 512 * 1024 {
                    controller_closed = true;
                }
                if let Some(end) = control_bytes.iter().position(|b| *b == b'\n') {
                    let frame: Vec<_> = control_bytes.drain(..=end).collect();
                    match serde_json::from_slice::<WriteArgs>(&frame) {
                        Ok(request) if request.validate().is_ok() && response_due.is_none() => {
                            input_bytes = request.chars.into_bytes();
                            input_offset = 0;
                            response_limit = request.max_output_tokens;
                            bytes.truncate(response_limit);
                            response_due =
                                Some(Instant::now() + Duration::from_millis(request.yield_time_ms));
                        }
                        _ => controller_closed = true,
                    }
                }
            }
            if input_offset < input_bytes.len() {
                match payload_input.write(&input_bytes[input_offset..]) {
                    Ok(n) => input_offset += n,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                    Err(_) => input_offset = input_bytes.len(),
                }
            }
            if graceful.is_none() && forced.is_none() {
                if controller_closed {
                    reason = "cancelled";
                    graceful = Some(Instant::now());
                } else if start.elapsed() >= Duration::from_millis(args.timeout_ms) {
                    reason = "timeout";
                    graceful = Some(Instant::now());
                } else if total > 10 * 1024 * 1024 {
                    reason = "output_limit";
                    graceful = Some(Instant::now());
                } else if exited.is_some() {
                    forced = Some(Instant::now());
                }
                if graceful.is_some() {
                    // pidfds pin identity against PID reuse. Supervisor remains outside.
                    if let Ok(members) = fs::read_to_string(&membership) {
                        for pid in members.lines().filter_map(|p| p.parse::<i32>().ok()) {
                            let pidfd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
                            if pidfd >= 0 {
                                // A PID might have been reused between listing the cgroup
                                // and opening the pidfd. Check membership after pinning it.
                                let expected = format!(
                                    "0::/{}",
                                    child_cg.strip_prefix("/sys/fs/cgroup").unwrap().display()
                                );
                                let belongs = fs::read_to_string(format!("/proc/{pid}/cgroup"))
                                    .is_ok_and(|s| s.lines().any(|l| l == expected));
                                if !belongs {
                                    unsafe {
                                        libc::close(pidfd as i32);
                                    }
                                    continue;
                                }
                                unsafe {
                                    libc::syscall(
                                        libc::SYS_pidfd_send_signal,
                                        pidfd,
                                        libc::SIGTERM,
                                        std::ptr::null::<libc::siginfo_t>(),
                                        0,
                                    );
                                    libc::close(pidfd as i32);
                                }
                            }
                        }
                    }
                }
            }
            if graceful.is_some_and(|t| t.elapsed() >= Duration::from_secs(2)) && forced.is_none() {
                forced = Some(Instant::now());
            }
            if forced.is_some() {
                let _ = (&kill).write_all(b"1");
            }
            let mut buffer = [0u8; 8192];
            pipes.retain_mut(|(stdout, pipe)| {
                // Bound draining per iteration so floods cannot starve cleanup/deadline.
                for _ in 0..16 {
                    match pipe.read(&mut buffer) {
                        Ok(0) => return false,
                        Ok(n) => {
                            let mut offset = 0;
                            if *stdout && marker_bytes.len() < marker.len() {
                                offset = n.min(marker.len() - marker_bytes.len());
                                marker_bytes.extend_from_slice(&buffer[..offset]);
                            }
                            let n = n - offset;
                            total = total.saturating_add(n);
                            chunk_total = chunk_total.saturating_add(n);
                            let keep = n.min(response_limit.saturating_sub(bytes.len()));
                            bytes.extend_from_slice(&buffer[offset..offset + keep]);
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                        Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                        Err(_) => {
                            pipes_ok = false;
                            return false;
                        }
                    }
                }
                true
            });
            if exited.is_some() && pipes.is_empty() && populated(&child_cg) == Ok(false) {
                cleanup_ok = pipes_ok;
                break;
            }
            if forced.is_some_and(|t| t.elapsed() >= Duration::from_secs(2)) {
                break;
            }
            if response_due.is_some_and(|due| Instant::now() >= due)
                && input_offset == input_bytes.len()
                && graceful.is_none()
                && forced.is_none()
                && marker_bytes == marker
            {
                println!(
                    "{}",
                    json!({"reason":"running","exit_code":null,"output":String::from_utf8_lossy(&bytes),"output_bytes":total,"truncated":chunk_total>bytes.len(),"cleanup_confirmed":false})
                );
                if std::io::stdout().flush().is_err() {
                    controller_closed = true;
                }
                bytes.clear();
                chunk_total = 0;
                response_limit = 16000;
                response_due = None;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        if marker_bytes != marker {
            reason = "launch_failure";
        }
        if total > 10 * 1024 * 1024 {
            reason = "output_limit";
        }
        let memory = fs::read_to_string(child_cg.join("memory.events")).unwrap_or_default();
        let pids = fs::read_to_string(child_cg.join("pids.events")).unwrap_or_default();
        if reason == "exit"
            && (memory
                .lines()
                .any(|l| l.starts_with("oom_kill ") && l != "oom_kill 0")
                || pids.lines().any(|l| l.starts_with("max ") && l != "max 0"))
        {
            reason = "resource_limit";
        }
        if !cleanup_ok {
            reason = "cleanup_failure";
        }
        if cleanup_ok && lease.confirm_cleanup().is_err() {
            cleanup_ok = false;
            reason = "cleanup_failure";
        }
        let _ = fs::remove_dir(&child_cg);
        Ok(
            json!({"reason":reason,"exit_code":if marker_bytes==marker {exited.and_then(|s|s.code())} else {None},"launched":marker_bytes==marker,"output":String::from_utf8_lossy(&bytes),"output_bytes":total,"truncated":chunk_total>bytes.len(),"cleanup_confirmed":cleanup_ok,"cgroup":child_cg,"memory_max":2147483648u64,"swap_max":0,"pids_max":128,"tmp_bytes":268435456,"memory_events":memory,"pids_events":pids}),
        )
    }
}
#[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
mod linux {
    pub fn run(_: &std::path::Path) -> Result<serde_json::Value, String> {
        Err("preflight_unsupported_platform".into())
    }
}
