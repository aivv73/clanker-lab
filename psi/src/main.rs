use psi::{
    core::{Plugin, Runtime},
    plugins::{BufferedResponsePlugin, FixtureModelPlugin, SingleResponsePlugin, StartupProbe},
    protocol::Controller,
};

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let mut directory = std::env::var_os("PSI_STATE_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            std::env::var_os("XDG_STATE_HOME")
                .map(std::path::PathBuf::from)
                .unwrap_or_else(|| {
                    std::path::PathBuf::from(std::env::var_os("HOME").expect("HOME required"))
                        .join(".local/state")
                })
                .join("psi/default")
        });
    let mut selection = "single-response".to_string();
    let mut gate = None;
    let mut fault = None;
    let mut model = "fixture".to_string();
    let mut oauth_fixture = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        let value = args.next().unwrap_or_default();
        match arg.as_str() {
            "--state-dir" => directory = value.into(),
            "--agent-loop" => selection = value,
            "--model" => model = value,
            "--oauth-fixture" => oauth_fixture = Some(value),
            "--fixture-gate" => gate = Some(std::path::PathBuf::from(value)),
            "--startup-fault" => fault = Some(value),
            _ => {
                eprintln!("unknown option: {arg}");
                std::process::exit(1);
            }
        }
    }
    if model.trim().is_empty() || (model == "fixture" && oauth_fixture.is_some()) {
        eprintln!("invalid model configuration");
        std::process::exit(1);
    }
    let agent: Box<dyn Plugin> = match selection.as_str() {
        "single-response" => Box::new(SingleResponsePlugin),
        "buffered-response" => Box::new(BufferedResponsePlugin),
        _ => {
            eprintln!("unknown agent loop: {selection}");
            std::process::exit(1);
        }
    };
    let provider: Box<dyn Plugin> = if model == "fixture" {
        Box::new(FixtureModelPlugin { gate })
    } else {
        let config = match oauth_fixture {
            Some(base) => match psi::codex::Config::fixture(&base, model.clone()) {
                Ok(config) => config,
                Err(error) => {
                    eprintln!("{error}");
                    std::process::exit(1);
                }
            },
            None => psi::codex::Config::live(model.clone()),
        };
        Box::new(psi::codex::CodexModelPlugin(config))
    };
    let mut plugins: Vec<Box<dyn Plugin>> = vec![agent, provider];
    if let Some(fault) = fault {
        let fault = match fault.as_str() {
            "missing" => "missing",
            "cycle" => "cycle",
            "duplicate" => "duplicate",
            "partial" => "partial",
            _ => {
                eprintln!("unknown startup fault");
                std::process::exit(1);
            }
        };
        plugins.push(Box::new(StartupProbe(fault)));
    }
    let mut runtime = match Runtime::start(plugins).await {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("Psi startup failed: {error}");
            std::process::exit(1);
        }
    };
    eprintln!("Psi runtime started");
    let result = match runtime.service("agent-loop") {
        Ok(agent) => match Controller::new(
            agent,
            runtime.work(),
            runtime.spawner("agent-loop").unwrap(),
            directory,
        ) {
            Ok(controller) => controller.with_model(model).serve().await,
            Err(error) => Err(error),
        },
        Err(error) => Err(error),
    };
    let cleanup = runtime.close().await;
    eprintln!("Psi runtime stopped");
    if let Err(error) = &result {
        eprintln!("Psi transport failed: {error}");
    }
    if let Err(error) = &cleanup {
        eprintln!("Psi shutdown failed: {error}");
    }
    if result.is_err() || cleanup.is_err() {
        std::process::exit(1);
    }
}
